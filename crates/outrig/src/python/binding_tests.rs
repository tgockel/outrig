//! The hosted-object transport, end to end on the host: an interpreter and binding processes
//! started by [`Relay`], the fixture library in each binding, and every request checked in the
//! binding.
//!
//! Nothing on the container side is enforcement, so the tests of a client written by hand send
//! RPyC requests past the interpreter's shim -- as agent code could, since it runs in the same
//! process as the transport's container end -- and expect the binding to refuse them with the
//! target untouched.

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::interpreter_tests::Interpreter;
use super::relay::{BINDING_SOURCE, Bind, Relay, Service, TIMEOUT, fixture_dir, rpyc_dir};
use super::testing::{Flag, Start, eventually, py, python};

// The programs' bounds, restated so that changing one changes a test.
const PART_MAX: usize = 512 * 1024;
const FRAME_MAX: usize = 128 << 20;
/// The longest `rpc` line either way: one part, base64-encoded, inside its JSON line.
const RPC_LINE_MAX: usize = 1 << 20;
/// How many connections a kernel may hold to one binding.
const POOL_MAX: usize = 4;

const FACTORY: &str = "outrig_fixture:make";

/// Python that binds `K` to `agent`'s kernel and `root` to its proxy of binding `fx`, then runs
/// `body`.
fn with_root_in(agent: &str, body: &str) -> String {
    format!(
        "import __main__\nAGENT = {agent:?}\nK = __main__._kernels[AGENT]\nroot = K.hosted('fx')\n{}",
        py(body)
    )
}

/// [`with_root_in`] for the primary kernel.
fn with_root(body: &str) -> String {
    with_root_in("primary", body)
}

/// Python that binds, after `root`: `pool`, the kernel's pool of connections to `fx`;
/// `numbers()` and `pending()`, the numbers of its connections and of those with a reply still
/// to come; `touch(path)`, which raises a flag; `appears(path)`, a coroutine that waits for one
/// without holding the loop; and `worker(fn)`, which runs `fn` on a thread of the loop's executor
/// at once -- `asyncio.to_thread`'s own mechanism, used where the kernel's thread is about to
/// block, since `to_thread` is a coroutine that submits nothing until it is awaited.
const POOL: &str = r#"
    import asyncio, json, os, threading, time
    pool = K._hosted['fx']
    loop = asyncio.get_running_loop()
    def numbers():
        return sorted(c.number for c in pool.connections)
    def pending():
        return sorted(c.number for c in pool.connections if c._request_callbacks)
    def touch(path):
        open(path, 'w').close()
    async def appears(path, timeout=20):
        deadline = time.monotonic() + timeout
        while not os.path.exists(path):
            if time.monotonic() > deadline:
                raise TimeoutError(f'{path} never appeared')
            await asyncio.sleep(0.01)
    def worker(fn):
        return loop.run_in_executor(None, fn)
"#;

/// [`with_root_in`], then [`POOL`], then `body`.
fn with_pool_in(agent: &str, body: &str) -> String {
    with_root_in(agent, &format!("{}\n{}", py(POOL), py(body)))
}

/// One interval the fixture recorded on the host: label, start, end, and the serving thread.
#[derive(Debug, Clone)]
struct Interval {
    label: String,
    start: f64,
    end: f64,
    thread: String,
}

impl Interval {
    fn overlaps(&self, other: &Interval) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// Every interval the binding `fx` has recorded, read through `agent`.
fn intervals(relay: &mut Relay, agent: &str, id: u64) -> Vec<Interval> {
    let out = relay.output_in(
        agent,
        id,
        &with_root_in(agent, "import json\nprint(json.dumps(root.intervals()))"),
    );
    let rows: Vec<Value> = serde_json::from_str(out.trim()).expect("intervals as JSON");
    rows.iter()
        .map(|row| Interval {
            label: row[0].as_str().expect("a label").to_string(),
            start: row[1].as_f64().expect("a start"),
            end: row[2].as_f64().expect("an end"),
            thread: row[3].as_str().expect("a thread name").to_string(),
        })
        .collect()
}

fn interval<'a>(list: &'a [Interval], label: &str) -> &'a Interval {
    list.iter()
        .find(|i| i.label == label)
        .unwrap_or_else(|| panic!("no interval {label:?} in {list:?}"))
}

/// The connection ids `agent` has used toward binding `fx`, as the relay saw them.
fn ids_used(relay: &Relay, agent: &str) -> Vec<u64> {
    relay
        .stats
        .between(agent, "fx")
        .connection_ids()
        .into_iter()
        .collect()
}

/// Python that binds `raw(handler, boxed)`: a request written by hand, past the shim, with its
/// arguments already boxed; `idp` for a proxy's id_pack; the labels; and `attempt`, which prints
/// what a call gave or how it was refused.
const RAW: &str = r#"
    import rpyc, struct, zlib
    from rpyc.core import consts
    conn = object.__getattribute__(root, "____conn__")
    idp = lambda proxy: object.__getattribute__(proxy, "____id_pack__")
    L, V, T, R = (consts.LABEL_LOCAL_REF, consts.LABEL_VALUE, consts.LABEL_TUPLE,
                  consts.LABEL_REMOTE_REF)
    def raw_on(on, handler, boxed):
        res = rpyc.core.async_.AsyncResult(on)
        seq = on._get_seq_id()
        on._request_callbacks[seq] = res
        on._send(consts.MSG_REQUEST, seq, (handler, boxed))
        return res.value
    def raw(handler, boxed):
        return raw_on(conn, handler, boxed)
    def attempt(label, fn):
        try:
            print(label, "->", repr(fn()))
        except Exception as e:
            print(label, "refused:", type(e).__name__, str(e).split("\n")[0])
    rid = idp(root)
"#;

/// [`with_root`], then [`RAW`], then `body`.
fn with_raw(body: &str) -> String {
    with_root(&format!("{}\n{}", py(RAW), py(body)))
}

/// Every line of `output` that starts with `label`.
fn lines_of<'a>(output: &'a str, label: &str) -> Vec<&'a str> {
    output
        .lines()
        .filter(|line| line.starts_with(label))
        .collect()
}

fn refused(output: &str, label: &str, reason: &str) {
    let lines = lines_of(output, label);
    assert_eq!(lines.len(), 1, "{label:?} in {output}");
    assert!(
        lines[0].contains("refused: outrig.Refused") && lines[0].contains(reason),
        "{label:?} was not refused for {reason:?}: {}",
        lines[0]
    );
}

// ---------------------------------------------------------------------------- the protocol

/// RPyC 6.0.2's request handlers, as the task's inventory names them. An id the vendored RPyC
/// has that this list lacks, or that the binding leaves to RPyC's own handler, fails here: an
/// upgrade cannot bring a request type in unexamined.
const INVENTORY: [&str; 20] = [
    "HANDLE_PING",
    "HANDLE_CLOSE",
    "HANDLE_GETROOT",
    "HANDLE_GETATTR",
    "HANDLE_DELATTR",
    "HANDLE_SETATTR",
    "HANDLE_CALL",
    "HANDLE_CALLATTR",
    "HANDLE_REPR",
    "HANDLE_STR",
    "HANDLE_CMP",
    "HANDLE_HASH",
    "HANDLE_DIR",
    "HANDLE_PICKLE",
    "HANDLE_DEL",
    "HANDLE_INSPECT",
    "HANDLE_BUFFITER",
    "HANDLE_OLDSLICING",
    "HANDLE_CTXEXIT",
    "HANDLE_INSTANCECHECK",
];

#[test]
fn every_request_type_is_in_the_table_and_the_inventory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("binding.py");
    std::fs::write(&source, BINDING_SOURCE).expect("write the binding program");
    let driver = py(r#"
        import json, sys
        rpyc_dir, source = sys.argv[1], sys.argv[2]
        sys.path.insert(0, rpyc_dir)
        import rpyc
        ns = {"__name__": "outrig_binding"}
        exec(compile(open(source).read(), source, "exec"), ns)
        Binding = ns["connection_class"](rpyc)
        handles = {n: v for n, v in vars(rpyc.core.consts).items() if n.startswith("HANDLE_")}
        stock = sorted(rpyc.core.protocol.Connection._request_handlers())
        ours = Binding._request_handlers()
        own = sorted(k for k, fn in ours.items() if "Binding." in fn.__qualname__)
        print(json.dumps({"handles": handles, "stock": stock, "ours": sorted(ours), "own": own}))
    "#);
    let output = Command::new(python())
        .args(["-I", "-c", &driver])
        .arg(rpyc_dir())
        .arg(&source)
        .output()
        .expect("the payload's Python runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("a JSON report");
    let handles = report["handles"]
        .as_object()
        .expect("the HANDLE_ constants");
    let mut named: Vec<&str> = handles.keys().map(String::as_str).collect();
    named.sort_unstable();
    let mut inventory = INVENTORY.to_vec();
    inventory.sort_unstable();
    assert_eq!(
        named, inventory,
        "the vendored RPyC's HANDLE_ constants and the inventory differ"
    );
    let mut ids: Vec<u64> = handles
        .values()
        .map(|v| v.as_u64().expect("an id"))
        .collect();
    ids.sort_unstable();
    let listed = |key: &str| -> Vec<u64> {
        report[key]
            .as_array()
            .expect("a list")
            .iter()
            .map(|v| v.as_u64().expect("an id"))
            .collect()
    };
    assert_eq!(
        ids,
        listed("stock"),
        "RPyC's own table and its constants differ"
    );
    assert_eq!(
        ids,
        listed("ours"),
        "the binding's table and RPyC's constants differ"
    );
    assert_eq!(
        ids,
        listed("own"),
        "a handler the binding leaves to RPyC's own"
    );
}

// ---------------------------------------------------------------------------- ordinary use

#[test]
fn ordinary_use_gives_the_hosts_answer() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            print(root.public)
            print(root.nested.value, root.nested.method())
            print(root.method(2, y=3))
            print(root.sequence[1], list(root.sequence))
            with root.manager as m:
                print(m.entered)
            print(root.manager.exits)
            try:
                with root.manager:
                    raise ValueError("inside", 5)
            except ValueError as e:
                print("raised", e.args, root.manager.exits[-1])
            print(root.takes(root.nested), root.takes(root))
            print(isinstance(root.nested, root.nested_type))
            root.writable = "after"
            print(root.writable)
            del root.writable
            print(hasattr(root, "writable"))
            "#,
        ),
    );
    assert_eq!(
        out,
        "public value\n\
         7 nested method\n\
         5\n\
         20 [10, 20, 30]\n\
         1\n\
         [(None, 'None', True)]\n\
         raised ('inside', 5) ('ValueError', \"ValueError('inside', 5)\", True)\n\
         True False\n\
         True\n\
         after\n\
         False\n"
    );
}

// ---------------------------------------------------------------------------- a client by hand

#[test]
fn a_comparison_or_a_target_the_rule_does_not_cover_is_refused() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            # CVE-2019-16328: a comparison naming `__getattribute__` as its operator.
            attempt("cmp_getattribute", lambda: raw(consts.HANDLE_CMP,
                (T, ((L, rid), (V, "_private"), (V, "__getattribute__")))))
            attempt("cmp_format", lambda: raw(consts.HANDLE_CMP, (T, ((L, rid), (L, rid), (V, "format")))))
            # A by-value target: `str.format` would read attributes through its format string.
            attempt("value target", lambda: raw(consts.HANDLE_CALLATTR,
                (T, ((V, "{0.__class__}"), (V, "format"), (T, ((L, rid),)), (V, ())))))
            attempt("str type", lambda: root.str_type.format("{0.__class__}", root))
            attempt("callable mark target", lambda: raw(consts.HANDLE_CALL,
                (T, ((105, ("outrig_session_primary.f", 1, 2)), (T, ()), (V, ())))))
            print("untouched", root.public, root.calls)
            "#,
        ),
    );
    refused(
        &out,
        "cmp_getattribute",
        "'__getattribute__' is not a comparison operator",
    );
    refused(&out, "cmp_format", "'format' is not a comparison operator");
    refused(
        &out,
        "value target",
        "not an object this binding handed out",
    );
    refused(
        &out,
        "str type",
        "str.format reads attributes through its format string",
    );
    refused(
        &out,
        "callable mark target",
        "not an object this binding handed out",
    );
    assert!(out.contains("untouched public value 0\n"), "{out}");
}

#[test]
fn copying_or_pickling_a_callback_sends_no_pickle_request() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            # CVE-2024-27758's path: a host method that copies or pickles a proxy it was given
            # would fetch pickle bytes from the client and `pickle.loads` them. The container's
            # pickle handler is replaced to see whether any such request arrives.
            pickles = []
            conn._HANDLERS = dict(conn._HANDLERS)
            conn._HANDLERS[consts.HANDLE_PICKLE] = lambda self, *a: pickles.append(a) or b"\x80\x04N."
            print(root.probe(lambda x: x))
            print("pickle requests", pickles, "host unpickles", root.unpickled())
            attempt("raw pickle", lambda: raw(consts.HANDLE_PICKLE, (T, ((L, rid), (V, 2)))))
            print("host unpickles", root.unpickled())
            "#,
        ),
    );
    assert!(
        out.contains("'pickle': ('TypeError', 'a callback cannot be pickled or copied"),
        "{out}"
    );
    assert!(
        out.contains("'copy': ('TypeError', 'a callback cannot be pickled or copied"),
        "{out}"
    );
    assert!(out.contains("'call': ('ok', '3')"), "{out}");
    assert!(
        out.contains("pickle requests [] host unpickles 0\n"),
        "{out}"
    );
    refused(&out, "raw pickle", "pickling is refused");
    assert!(out.ends_with("host unpickles 0\n"), "{out}");
}

#[test]
fn names_that_reach_the_interpreter_are_refused() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            method = root.method
            attempt("globals", lambda: method.__globals__)
            attempt("mro", lambda: root.nested_type.__mro__)
            attempt("subclasses", lambda: root.nested_type.__subclasses__())
            attempt("dict", lambda: root.__dict__)
            attempt("class", lambda: raw(consts.HANDLE_GETATTR, (T, ((L, rid), (V, "__class__")))))
            attempt("attr_private", lambda: root._private)
            attempt("method_private", lambda: root._private_method())
            attempt("setattr", lambda: setattr(root, "_private", "x"))
            attempt("delattr", lambda: delattr(root, "_private"))
            attempt("gi_frame", lambda: root.gen().gi_frame)
            attempt("frames", lambda: root.frames())
            safe = rpyc.core.protocol.DEFAULT_CONFIG["safe_attrs"]
            print("dir", [n for n in dir(root) if n.startswith("_") and n not in safe])
            print("still", root.method(1))
            "#,
        ),
    );
    for (label, name) in [
        ("globals", "__globals__"),
        ("mro", "__mro__"),
        ("subclasses", "__subclasses__"),
        ("dict", "__dict__"),
        ("class", "__class__"),
        ("attr_private", "_private"),
        ("method_private", "_private_method"),
        ("setattr", "_private"),
        ("delattr", "_private"),
    ] {
        refused(&out, label, &format!("'{name}' is not a public name"));
    }
    refused(&out, "gi_frame", "a frame object does not cross");
    refused(&out, "frames", "a frame object does not cross");
    assert!(out.contains("dir []\n"), "{out}");
    assert!(out.ends_with("still 2\n"), "{out}");
}

#[test]
fn inspect_answers_public_names_and_call_only() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            safe = rpyc.core.protocol.DEFAULT_CONFIG["safe_attrs"]
            names = sorted(n for n, _ in raw(consts.HANDLE_INSPECT, (T, ((V, idp(root.callable)),))))
            print("has", "__call__" in names, "public" in names, "_private" in names)
            print("outside", [n for n in names if n != "__call__" and n.startswith("_") and n not in safe])
            plain = sorted(n for n, _ in raw(consts.HANDLE_INSPECT, (T, ((V, idp(root.nested)),))))
            print("plain has call", "__call__" in plain)
            print("called", root.callable(21))
            "#,
        ),
    );
    assert_eq!(
        out,
        "has True True False\noutside []\nplain has call False\ncalled 42\n"
    );
}

#[test]
fn a_release_past_the_references_handed_out_is_refused() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            nested = root.nested
            attempt("inflated", lambda: raw(consts.HANDLE_DEL, (T, ((L, idp(nested)), (V, 1000)))))
            attempt("zero", lambda: raw(consts.HANDLE_DEL, (T, ((L, idp(nested)), (V, 0)))))
            attempt("unknown", lambda: raw(consts.HANDLE_DEL, (T, ((L, ("x.Y", 1, 2)), (V, 1)))))
            print("reachable", nested.value)
            "#,
        ),
    );
    refused(
        &out,
        "inflated",
        "a release of 1000 references to an object this connection handed out 1 times",
    );
    refused(&out, "zero", "a release of 0 references");
    refused(&out, "unknown", "never handed out");
    assert!(out.ends_with("reachable 7\n"), "{out}");
}

#[test]
fn a_callback_answered_with_a_reference_is_refused() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            # The container's handlers are replaced to see whether the host requests anything
            # through the reference it was given; the shim is bypassed for the reply alone.
            marker = object()
            seen = []
            conn._HANDLERS = {
                k: (lambda f: lambda self, *a: seen.append(a) or f(self, *a))(f)
                for k, f in conn._HANDLERS.items()
            }
            real_box = conn._box
            for label in (R, 105):
                def fn():
                    # Boxes the reply once as a reference of the given label, then steps aside.
                    def once(obj, seen=frozenset()):
                        conn._box = real_box
                        return (label, rpyc.lib.get_id_pack(obj))
                    conn._box = once
                    conn._local_objects.add(rpyc.lib.get_id_pack(marker), marker)
                    return marker
                try:
                    attempt(f"reply {label}", lambda: root.call(fn))
                    print(f"host call {label} raised", root.calling_raised(fn))
                finally:
                    conn._box = real_box
            print("through the marker", [a for a in seen if any(x is marker for x in a)])
            "#,
        ),
    );
    refused(
        &out,
        "reply 4",
        "an object of type builtins.object was sent by reference",
    );
    refused(
        &out,
        "reply 105",
        "a callable of type builtins.object was returned from a callback",
    );
    assert!(out.contains("host call 4 raised Refused\n"), "{out}");
    assert!(out.contains("host call 105 raised Refused\n"), "{out}");
    assert!(out.ends_with("through the marker []\n"), "{out}");
}

#[test]
fn a_request_type_outside_the_table_is_refused() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            attempt("unknown", lambda: raw(99, (T, ())))
            attempt("bool", lambda: raw(True, (T, ())))
            print("still", root.public)
            "#,
        ),
    );
    refused(
        &out,
        "unknown",
        "request type 99 is not one this binding answers",
    );
    refused(
        &out,
        "bool",
        "request type True is not one this binding answers",
    );
    assert!(out.ends_with("still public value\n"), "{out}");
}

#[test]
fn a_compressed_frame_closes_the_connection_which_reopens() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            data = zlib.compress(b"x" * 5000)
            conn._channel._write(struct.pack("!LB", len(data), 1) + data + b"\n", False)
            try:
                root.public
            except EOFError as e:
                print("closed:", e)
            root = __main__._kernels['primary'].hosted('fx')
            print("reopened", root.public, object.__getattribute__(root, "____conn__") is not conn)
            "#,
        ),
    );
    assert!(
        out.starts_with("closed: the host closed this connection: a compressed frame"),
        "{out}"
    );
    assert!(out.ends_with("reopened public value True\n"), "{out}");
}

// ---------------------------------------------------------------------------- what crosses

#[test]
fn containers_arrive_as_builtins() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            import pathlib
            print(root.types([1], {"a": 1}, {1}, pathlib.Path("/tmp/x"), frozenset({root.nested})))
            print(root.shape([{"k": [1, {2}]}, pathlib.Path("/x"), (root.nested, b"b")]))
            print(root.call(lambda p: p, pathlib.Path("/tmp/x")) == "/tmp/x")
            "#,
        ),
    );
    assert_eq!(
        out,
        "('list', 'dict', 'set', 'str', 'frozenset')\n\
         list[str, tuple[Nested, bytes], {str: list[int, set[int]]}]\n\
         True\n"
    );
}

#[test]
fn a_subclass_of_a_by_value_type_crosses_as_its_base_value() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            import collections, enum
            class Flag(enum.IntEnum):
                A = 1
                B = 2
            Point = collections.namedtuple("Point", "x y")
            class Name(str):
                def __str__(self):
                    return "overridden"
            class Floaty(float):
                pass
            print(root.types(Flag.A, Point(1, 2), Name("n"), Floaty(1.5),
                             collections.OrderedDict(a=1), collections.defaultdict(list, b=[]),
                             collections.Counter("aab")))
            print(root.method(Flag.A, y=Flag.B), root.shape(Point(1, "a")), root.call(str, Name("n")))
            "#,
        ),
    );
    assert_eq!(
        out,
        "('int', 'tuple', 'str', 'float', 'dict', 'dict', 'dict')\n3 tuple[int, str] n\n"
    );
}

#[test]
fn a_container_object_sent_by_reference_is_refused() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            class Own:
                pass
            attempt("shim", lambda: root.noted(Own()))
            attempt("generator", lambda: root.noted(x for x in range(3)))
            attempt("raw", lambda: raw(consts.HANDLE_CALLATTR,
                (T, ((L, rid), (V, "noted"), (T, ((R, ("outrig_session_primary.Own", 1, 2)),)), (V, ())))))
            print("calls", root.calls)
            "#,
        ),
    );
    let shim = lines_of(&out, "shim");
    assert!(
        shim[0].contains(
            "refused: TypeError an object of type Own cannot be passed to a hosted object"
        ),
        "{out}"
    );
    assert!(
        lines_of(&out, "generator")[0].contains("an object of type generator cannot be passed"),
        "{out}"
    );
    refused(
        &out,
        "raw",
        "an object of type outrig_session_primary.Own was sent by reference",
    );
    assert!(out.ends_with("calls 0\n"), "{out}");
}

#[test]
fn another_bindings_or_kernels_proxy_is_refused_before_anything_is_sent() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.bind("fx2", FACTORY);
    relay.open("child");
    relay.output_in(
        "child",
        1,
        &py(r#"
            import __main__
            __main__.from_child = __main__._kernels["child"].hosted("fx").nested
        "#),
    );
    relay.output(
        2,
        &with_root(
            r#"
            nested = root.nested
            other = __main__._kernels["primary"].hosted("fx2")
            # Bound ahead of the count below: reading a method is a request of its own.
            __main__.nested, __main__.other = nested, other
            __main__.other_takes, __main__.root_takes = other.takes, root.takes
            "#,
        ),
    );
    let before = relay.stats.to_binding.lines();
    let out = relay.output(
        3,
        &py(r#"
            import __main__
            root = __main__._kernels["primary"].hosted("fx")
            for label, takes, proxy in (("binding", __main__.other_takes, __main__.nested),
                                        ("kernel", __main__.root_takes, __main__.from_child)):
                try:
                    takes(proxy)
                except TypeError as e:
                    print(label, "refused:", e)
        "#),
    );
    assert!(
        out.contains(
            "binding refused: a proxy of binding 'fx' on agent 'primary' cannot be passed to \
             binding 'fx2' on agent 'primary'"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "kernel refused: a proxy of binding 'fx' on agent 'child' cannot be passed to \
             binding 'fx' on agent 'primary'"
        ),
        "{out}"
    );
    assert_eq!(
        relay.stats.to_binding.lines(),
        before,
        "a line crossed for a refused argument"
    );
    assert_eq!(
        relay.output(4, "print(__main__.other.calls, root.calls)"),
        "0 0\n"
    );
}

// ---------------------------------------------------------------------------- callbacks

#[test]
fn a_callback_works_only_during_its_call() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            ran = []
            def fn(x):
                ran.append(x)
                return x + 1
            print(root.call(fn, 41), root.probe(fn))
            root.keep(fn)
            try:
                root.call_kept()
            except Exception as e:
                print("later:", type(e).__name__, str(e).split("\n")[0])
            print("ran", ran)
            "#,
        ),
    );
    assert!(
        out.starts_with("42 {'attr': ('AttributeError', \"a callback has no attribute 'anything'"),
        "{out}"
    );
    assert!(
        out.contains("'repr': ('TypeError', 'a callback has no repr"),
        "{out}"
    );
    assert!(
        out.contains("'pickle': ('TypeError', 'a callback cannot be pickled or copied"),
        "{out}"
    );
    assert!(
        out.contains("'copy': ('TypeError', 'a callback cannot be pickled or copied"),
        "{out}"
    );
    assert!(out.contains("'call': ('ok', '4')"), "{out}");
    assert!(
        out.contains("later: outrig.Refused this callback cannot be called: the request it was passed to has returned"),
        "{out}"
    );
    assert!(out.ends_with("ran [41, 3]\n"), "{out}");
}

#[test]
fn a_callbacks_return_value_crosses_by_the_argument_rules() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            class Own:
                pass
            print(root.returned_type(lambda: [1, 2]), root.returned_type(lambda: root.nested))
            print(root.calling_raised(lambda: Own()), root.calling_raised(lambda: (lambda: 1)))
            try:
                root.returned_type(lambda: Own())
            except Exception as e:
                print("raised:", type(e).__name__, str(e).split("\n")[0])
            "#,
        ),
    );
    assert!(
        out.starts_with("list Nested\nTypeError TypeError\n"),
        "{out}"
    );
    assert!(
        out.contains("raised: TypeError an object of type Own cannot be passed to a hosted object"),
        "{out}"
    );
}

// ---------------------------------------------------------------------------- exceptions

#[test]
fn exceptions_arrive_typed_where_their_module_imports() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(&format!(
            r#"
            import sys
            try:
                root.raise_fixture()
            except Exception as e:
                print("generic:", type(e).__name__, type(e).__module__, e.args, e.status,
                      "File" in str(e), "Traceback (most recent" in str(e))
            sys.path.append({fixture:?})
            import outrig_fixture
            try:
                root.raise_fixture()
            except outrig_fixture.FixtureError as e:
                print("typed:", e.args, e.status, "File" in str(e))
            "#,
            fixture = fixture_dir()
        )),
    );
    assert_eq!(
        out,
        "generic: outrig_fixture.FixtureError rpyc.core.vinegar/outrig_fixture ('fixture failed',) \
         3 False False\n\
         typed: ('fixture failed',) 3 False\n"
    );
}

#[test]
fn a_rebuilt_exception_says_what_the_host_said() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(&format!(
            r#"
            import sys
            sys.path.append({fixture:?})
            import outrig_fixture
            try:
                root.raise_command()
            except outrig_fixture.CommandError as e:
                print(repr(str(e)))
            "#,
            fixture = fixture_dir()
        )),
    );
    // The class's own `__str__` reads a private attribute the rebuilt exception lacks, so RPyC
    // prints it as unprintable; the host's one-line rendering follows, and no frame.
    assert!(
        out.contains("outrig_fixture.CommandError: command 'git push' failed with status 128"),
        "{out}"
    );
    assert!(
        !out.contains("File \"") && !out.contains("Traceback (most recent"),
        "{out}"
    );
}

#[test]
fn the_host_imports_nothing_the_container_names() {
    let marker = tempfile::tempdir().expect("tempdir");
    let marker = marker.path().join("imported");
    let marker_text = marker.to_str().expect("a UTF-8 path");
    // The module writes the marker when imported, and the binding process could import it.
    let probe = Command::new(python())
        .args([
            "-I",
            "-c",
            "import sys; sys.path.append(sys.argv[1]); import fixture_evil",
        ])
        .arg(fixture_dir())
        .env("OUTRIG_FIXTURE_EVIL_MARKER", marker_text)
        .status()
        .expect("the payload's Python runs");
    assert!(
        probe.success() && marker.exists(),
        "fixture_evil does not import"
    );
    std::fs::remove_file(&marker).expect("remove the marker");

    let mut relay = Relay::start();
    relay.bind_with(
        "fx",
        FACTORY,
        &[("OUTRIG_FIXTURE_EVIL_MARKER", marker_text)],
    );
    let out = relay.output(
        1,
        &with_root(
            r#"
            class Evil(Exception):
                __module__ = "fixture_evil"
            def raiser():
                raise Evil("claims a module the host could import")
            print(root.calling_raised(raiser))
            "#,
        ),
    );
    assert_eq!(out, "ContainerError\n");
    assert!(!marker.exists(), "the host imported fixture_evil");

    // An interrupt raised inside a callback answers the host -- as an `Exception`, so the
    // library's cleanup runs -- and then ends the execution that was making the call.
    let result = relay.exec(
        2,
        &with_root(
            r#"
            class Interrupt(KeyboardInterrupt):
                pass
            def interrupter():
                raise Interrupt()
            root.calling_raised(interrupter)
            print("not reached")
            "#,
        ),
    );
    assert_eq!(result["status"], "error", "{result}");
    assert_eq!(result["raised"], "Interrupt", "{result}");
    assert_eq!(result["output"], "", "{result}");
    assert_eq!(
        relay.output(3, &with_root("print(root.last_raised)")),
        "ContainerError\n"
    );
}

// ---------------------------------------------------------------------------- bounds

#[test]
fn a_large_result_is_bounded_in_transit() {
    let mut relay = Relay::start_with(&Start {
        ceiling: Some(512 << 20),
        ..Start::default()
    });
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(&format!(
            r#"
            import hashlib, resource
            data = root.big(64 << 20)
            print(type(data).__name__, len(data))
            del data
            attempt("too big", lambda: root.big({FRAME_MAX} + 1))
            print("peak MiB", resource.getrusage(resource.RUSAGE_SELF).ru_maxrss // 1024)
            __main__.root1 = root
            "#
        )),
    );
    // Shown under `--nocapture`, for the record of what the ceiling left: the interpreter's
    // peak resident size after the result crossed.
    println!("{out}");
    assert!(out.starts_with("bytes 67108864\n"), "{out}");
    refused(
        &out,
        "too big",
        &format!("is past the {FRAME_MAX}-byte frame bound"),
    );
    assert!(
        relay.stats.to_interpreter.longest() <= RPC_LINE_MAX
            && relay.stats.to_binding.longest() <= RPC_LINE_MAX,
        "a line over the bound: {} toward the interpreter, {} toward the binding",
        relay.stats.to_interpreter.longest(),
        relay.stats.to_binding.longest()
    );
    assert!(
        relay.stats.to_interpreter.longest() > PART_MAX,
        "the result did not cross in full parts: {}",
        relay.stats.to_interpreter.longest()
    );

    // The container's own sender refuses before a line crosses.
    let before = relay.stats.to_binding.lines();
    let out = relay.output(
        2,
        &with_raw(&format!(
            "attempt('send', lambda: conn._channel.send(b'x' * ({FRAME_MAX} + 1)))"
        )),
    );
    assert!(
        lines_of(&out, "send")[0].contains(&format!(
            "refused: ValueError a frame of {} bytes is past",
            FRAME_MAX + 1
        )),
        "{out}"
    );
    assert_eq!(relay.stats.to_binding.lines(), before);

    // A frame past the bound toward the interpreter, written as the host would relay it: the
    // connection closes with the reason, and the next use says so, then reopens.
    let header = {
        let mut frame = (FRAME_MAX as u32 + 1).to_be_bytes().to_vec();
        frame.push(0);
        frame.extend_from_slice(b"some data");
        frame
    };
    relay.send(json!({
        "t": "rpc", "agent": "primary", "binding": "fx", "id": 1,
        "data": base64(&header), "more": true,
    }));
    let out = relay.output(
        3,
        &py(r#"
            import __main__
            try:
                __main__.root1.public
            except EOFError as e:
                print("closed:", e)
            root = __main__._kernels["primary"].hosted("fx")
            __main__.root3 = root
            print(root.public, object.__getattribute__(root, "____conn__").number)
        "#),
    );
    assert!(
        out.starts_with(&format!(
            "closed: a frame of {} bytes is past the {FRAME_MAX}-byte bound\n",
            FRAME_MAX + 1
        )),
        "{out}"
    );
    assert!(out.ends_with("public value 2\n"), "{out}");

    // And toward the binding: a part past the part bound closes that connection, and the kernel
    // learns why.
    relay.inject_to_binding(
        "fx",
        json!({
            "t": "rpc", "agent": "primary", "id": 2,
            "data": base64(&vec![b'x'; PART_MAX + 1]), "more": true,
        }),
    );
    let out = relay.output(
        4,
        &py(r#"
            import __main__
            try:
                print(__main__.root3.public)
            except EOFError as e:
                print("closed:", e)
        "#),
    );
    assert!(
        out.starts_with(&format!(
            "closed: the host closed this connection: a part of {} bytes is past the {PART_MAX}-byte bound",
            PART_MAX + 1
        )),
        "{out}"
    );
}

// ---------------------------------------------------------------------------- after review

#[test]
fn a_context_manager_exits_for_an_exception_the_host_cannot_rebuild() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            # An exception group's `__new__` needs arguments, so it cannot be rebuilt by name on
            # the host; the library's `__exit__` still runs, with the plain exception that stands
            # in. A `UnicodeDecodeError` checks its arguments in `__init__` and rebuilds as itself.
            for raised in (ExceptionGroup("g", [ValueError("x")]),
                           UnicodeDecodeError("utf-8", b"\xff", 0, 1, "bad")):
                try:
                    with root.manager:
                        raise raised
                except type(raised):
                    pass
                exit = root.manager.exits[-1]
                print(exit[0], type(raised).__name__ in exit[1])
            print(root.manager.entered, len(root.manager.exits))
            "#,
        ),
    );
    assert_eq!(out, "ContainerError True\nUnicodeDecodeError True\n2 2\n");
}

#[test]
fn a_callback_the_library_kept_cannot_be_read_back() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            root.keep(lambda x: x)
            try:
                root.kept
            except Exception as e:
                print("refused:", type(e).__name__, str(e).split("\n")[0])
            print(root.public)
            "#,
        ),
    );
    assert_eq!(
        out,
        "refused: outrig.Refused a callback cannot be read back once the call it was passed to \
         has returned\n\
         public value\n"
    );
}

#[test]
fn a_part_before_a_frames_last_must_be_full() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    // Toward the binding, by hand: a header alone, with more to come.
    let out = relay.output(
        1,
        &with_raw(
            r#"
            conn._channel._write(struct.pack("!LB", 10, 0), True)
            try:
                root.public
            except EOFError as e:
                print("closed:", e)
            root = __main__._kernels["primary"].hosted("fx")
            __main__.root2 = root
            print(root.public, object.__getattribute__(root, "____conn__").number)
            "#,
        ),
    );
    assert_eq!(
        out,
        format!(
            "closed: the host closed this connection: a part before a frame's last carries \
             {PART_MAX} bytes, not 5\npublic value 2\n"
        )
    );
    // Toward the interpreter, as the host would relay it.
    relay.send(json!({
        "t": "rpc", "agent": "primary", "binding": "fx", "id": 2,
        "data": base64(&[0, 0, 0, 10, 0]), "more": true,
    }));
    let out = relay.output(
        2,
        &py(r#"
            import __main__
            try:
                __main__.root2.public
            except EOFError as e:
                print("closed:", e)
        "#),
    );
    assert_eq!(
        out,
        format!("closed: a part before a frame's last carries {PART_MAX} bytes, not 5\n")
    );
}

#[test]
fn a_reply_that_is_refused_keeps_no_reference() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            # `nested` was boxed before the frame beside it was refused: the reply never went
            # out, so the one reference the binding holds afterwards is the read below.
            attempt("frames", lambda: root.frames_with_nested())
            nested = root.nested
            attempt("two", lambda: raw(consts.HANDLE_DEL, (T, ((L, idp(nested)), (V, 2)))))
            attempt("one", lambda: raw(consts.HANDLE_DEL, (T, ((L, idp(nested)), (V, 1)))))
            "#,
        ),
    );
    refused(&out, "frames", "a frame object does not cross");
    refused(
        &out,
        "two",
        "a release of 2 references to an object this connection handed out 1 times",
    );
    assert!(lines_of(&out, "one")[0].ends_with("-> None"), "{out}");
}

#[test]
fn a_torn_send_tells_the_binding() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let before = relay.stats.closed_to_binding();
    let out = relay.output(
        1,
        &with_raw(&format!(
            r#"
            real_write = conn._channel._write
            def failing(part, more):
                if more:
                    raise OSError("the pipe tore")
                real_write(part, more)
            conn._channel._write = failing
            try:
                root.method(b"x" * {PART_MAX})
            except OSError as e:
                print("torn:", e)
            print(conn.closed)
            root = __main__._kernels["primary"].hosted("fx")
            print(root.public, object.__getattribute__(root, "____conn__").number)
            "#
        )),
    );
    assert_eq!(out, "torn: the pipe tore\nTrue\npublic value 2\n");
    assert_eq!(
        relay.stats.closed_to_binding(),
        before + 1,
        "the binding was not told of the torn connection"
    );
}

#[test]
fn add_note_survives_the_crossing() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            try:
                root.raise_fixture()
            except Exception as e:
                e.add_note("noted in the container")
                print(e.__notes__, callable(e.add_note))
            print(root.note_raised(lambda: 1 / 0))
            "#,
        ),
    );
    assert_eq!(
        out,
        "['noted in the container'] True\n('ZeroDivisionError', ['noted on the host'])\n"
    );
}

#[test]
fn a_callback_argument_past_the_bound_raises_where_it_was_sent() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(&format!(
            r#"
            nested = root.nested
            print(root.call_with_bytes(len, 1000))
            # The library's own call raises, in its thread, and nothing else on the connection
            # is touched: the proxy delivered before still answers.
            print(root.call_with_bytes(len, {FRAME_MAX} + 1))
            print(nested.value, root.public)
            "#
        )),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert_eq!(lines[0], "1000");
    assert!(
        lines[1].starts_with("ValueError: a frame of ")
            && lines[1].ends_with(&format!("bytes is past the {FRAME_MAX}-byte bound")),
        "{out}"
    );
    assert_eq!(lines[2], "7 public value");
}

#[test]
fn a_frame_that_cannot_be_held_fails_its_call() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            # The channel's `put` runs out of memory on the reply, once more on the reserve too.
            real_put = conn._channel.put
            attempts = []
            def failing(part, more):
                attempts.append(more)
                if len(attempts) > 2:
                    conn._channel.put = real_put
                    return real_put(part, more)
                raise MemoryError
            conn._channel.put = failing
            try:
                root.public
            except EOFError as e:
                print("closed:", e, len(attempts))
            root = __main__._kernels["primary"].hosted("fx")
            print(root.public, object.__getattribute__(root, "____conn__").number)
            "#,
        ),
    );
    assert_eq!(
        out,
        "closed: no memory to hold a frame from the binding 2\npublic value 2\n"
    );
}

/// Standard base64 with padding, as the programs decode it.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let bits =
            chunk.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ALPHABET[((bits >> (18 - 6 * i)) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------- the import

#[test]
fn the_interpreter_imports_rpyc_from_its_own_directory() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        workspace.path().join("rpyc.py"),
        b"raise SystemExit('the workspace copy ran')\n",
    )
    .expect("write a shadowing module");
    let mut relay = Relay::start_with(&Start {
        dir: Some(workspace.path()),
        ..Start::default()
    });
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(&format!(
            r#"
            import sys, rpyc
            print(rpyc.__file__.startswith({rpyc:?}), sys.modules["rpyc"] is rpyc)
            print(root.public)
            "#,
            rpyc = rpyc_dir()
        )),
    );
    assert_eq!(out, "True True\npublic value\n");
}

#[test]
fn without_rpyc_the_interpreter_still_runs_and_says_so() {
    // Started as `host.rs` starts it today, with no directory: everything else works.
    let mut interpreter = Interpreter::start();
    let result = interpreter.exec(
        1,
        "import __main__\n__main__._kernels['primary'].hosted('fx')",
    );
    assert_eq!(result["status"], "error");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("started without RPyC")),
        "{result}"
    );
    assert_eq!(interpreter.output(2, "1 + 1"), "2\n");
}

// ---------------------------------------------------------------------------- threading

#[test]
fn another_kernel_runs_while_one_blocks_in_a_hosted_call() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    let (started, release) = (Flag::new(), Flag::new());
    // A's thread blocks in the call -- until released, ten seconds at most -- and its loop with
    // it.
    relay.submit_in(
        "primary",
        1,
        &with_root(&format!(
            "print(root.record(10, 'a', started={}, until={}))",
            started.py(),
            release.py()
        )),
    );
    started.wait();
    // B runs executions to completion and answers `inv` meanwhile.
    assert_eq!(
        relay.output_in("b", 1, &with_root_in("b", "print(root.method(1))")),
        "2\n"
    );
    assert!(relay.inv("b", 2)["globals"].is_array());
    // The reader thread answers A's `cpu` and `msg`.
    assert!(relay.cpu("primary", 3)["seconds"].is_number());
    assert_eq!(relay.msg("primary", 4, "user", "hello")["pending"], 1);
    // A's `inv` is its loop's to answer, and the loop is not turning: B's next result comes
    // first.
    relay.ask_inv("primary", 5);
    relay.submit_in("b", 6, "print('still running')");
    let next = relay.recv();
    assert!(
        next["t"] == "result" && next["agent"] == "b" && next["id"] == 6,
        "expected B's result before A's inventory, got: {next}"
    );
    release.raise();
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["output"], "a\n");
    assert!(relay.inv_answer("primary", 5, TIMEOUT)["globals"].is_array());
}

#[test]
fn a_callback_runs_on_the_calling_kernels_thread() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    relay.open("c");
    let (started, release) = (Flag::new(), Flag::new());
    relay.submit_in(
        "b",
        1,
        &with_root_in(
            "b",
            &format!(
                "root.record(0, 'b', started={}, until={})",
                started.py(),
                release.py()
            ),
        ),
    );
    started.wait();
    let callback = r#"
        import threading
        seen = []
        def fn(x):
            seen.append((threading.current_thread() is threading.main_thread(),
                         threading.current_thread().name))
            return x + 1
        print(root.call(fn, 1), seen)
    "#;
    // While B's call is in flight to the same binding, each kernel's callback runs on the
    // thread that made the call: the primary's on the main thread, a child's on its own.
    assert_eq!(
        relay.output(1, &with_root(callback)),
        "2 [(True, 'MainThread')]\n"
    );
    assert_eq!(
        relay.output_in("c", 1, &with_root_in("c", callback)),
        "2 [(False, 'kernel-c')]\n"
    );
    release.raise();
    assert_eq!(relay.result_from("b", 1, TIMEOUT)["status"], "ok");
}

#[test]
fn two_kernels_calls_overlap_on_the_host() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    let flags: Vec<Flag> = (0..4).map(|_| Flag::new()).collect();
    let (sa, sb, fa, fb) = (&flags[0], &flags[1], &flags[2], &flags[3]);
    relay.submit_in(
        "primary",
        1,
        &with_root(&format!(
            "print(root.record(0, 'a', started={}, until={}))",
            sa.py(),
            fa.py()
        )),
    );
    relay.submit_in(
        "b",
        1,
        &with_root_in(
            "b",
            &format!(
                "print(root.record(0, 'b', started={}, until={}))",
                sb.py(),
                fb.py()
            ),
        ),
    );
    // Both calls are running on the host before either is released.
    sa.wait();
    sb.wait();
    fa.raise();
    fb.raise();
    assert_eq!(relay.result_from("primary", 1, TIMEOUT)["output"], "a\n");
    assert_eq!(relay.result_from("b", 1, TIMEOUT)["output"], "b\n");
    let recorded = intervals(&mut relay, "primary", 2);
    let (a, b) = (interval(&recorded, "a"), interval(&recorded, "b"));
    assert!(a.overlaps(b), "{recorded:?}");
    assert_eq!(
        (a.thread.as_str(), b.thread.as_str()),
        ("serve-primary-1", "serve-b-1")
    );
}

#[test]
fn two_workers_calls_from_one_kernel_overlap_and_use_two_connections() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let flags: Vec<Flag> = (0..5).map(|_| Flag::new()).collect();
    let (s1, s2, f1, f2, both) = (&flags[0], &flags[1], &flags[2], &flags[3], &flags[4]);
    // The loop stays free here, so the workers are `asyncio.to_thread` as the orientation
    // spells it, with the whole expression in the worker.
    relay.submit_in(
        "primary",
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                w1 = asyncio.ensure_future(asyncio.to_thread(
                    lambda: root.record(0, 'w1', started={s1}, until={f1})))
                w2 = asyncio.ensure_future(asyncio.to_thread(
                    lambda: root.record(0, 'w2', started={s2}, until={f2})))
                await appears({s1})
                await appears({s2})
                touch({both})
                print(await asyncio.gather(w1, w2), numbers())
                "#,
                s1 = s1.py(),
                s2 = s2.py(),
                f1 = f1.py(),
                f2 = f2.py(),
                both = both.py()
            ),
        ),
    );
    both.wait();
    assert_eq!(ids_used(&relay, "primary"), [1, 2]);
    f1.raise();
    f2.raise();
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["output"], "['w1', 'w2'] [1, 2]\n", "{result}");
    let recorded = intervals(&mut relay, "primary", 2);
    let (w1, w2) = (interval(&recorded, "w1"), interval(&recorded, "w2"));
    assert!(w1.overlaps(w2), "{recorded:?}");
    let mut threads = vec![w1.thread.clone(), w2.thread.clone()];
    threads.sort();
    assert_eq!(threads, ["serve-primary-1", "serve-primary-2"]);
    assert_eq!(ids_used(&relay, "primary"), [1, 2]);
}

#[test]
fn a_fifth_call_waits_for_a_free_connection() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let starts: Vec<Flag> = (0..4).map(|_| Flag::new()).collect();
    let releases: Vec<Flag> = (0..4).map(|_| Flag::new()).collect();
    let (fifth_started, four) = (Flag::new(), Flag::new());
    let list = |flags: &[Flag]| {
        let items: Vec<String> = flags.iter().map(Flag::py).collect();
        format!("[{}]", items.join(", "))
    };
    relay.submit_in(
        "primary",
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                S, F = {starts}, {releases}
                ws = [worker(lambda i=i: root.record(0, f'w{{i}}', started=S[i], until=F[i]))
                      for i in range(4)]
                for s in S:
                    await appears(s)
                fifth = worker(lambda: root.record(0, 'w5', started={s5}))
                touch({four})
                print(sorted(await asyncio.gather(*ws, fifth)), numbers())
                "#,
                starts = list(&starts),
                releases = list(&releases),
                s5 = fifth_started.py(),
                four = four.py()
            ),
        ),
    );
    four.wait();
    // Four in flight, and the fifth waiting: no fifth connection exists for it.
    let all: Vec<u64> = (1..=POOL_MAX as u64).collect();
    assert_eq!(ids_used(&relay, "primary"), all);
    assert!(!fifth_started.exists());
    releases[0].raise();
    fifth_started.wait();
    for release in &releases[1..] {
        release.raise();
    }
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(
        result["output"], "['w0', 'w1', 'w2', 'w3', 'w5'] [1, 2, 3, 4]\n",
        "{result}"
    );
    assert_eq!(ids_used(&relay, "primary"), [1, 2, 3, 4]);
    let recorded = intervals(&mut relay, "primary", 2);
    let first_end = (0..4)
        .map(|i| interval(&recorded, &format!("w{i}")).end)
        .fold(f64::INFINITY, f64::min);
    assert!(interval(&recorded, "w5").start >= first_end, "{recorded:?}");
}

#[test]
fn a_proxys_request_travels_on_any_free_connection() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (held, release, done) = (Flag::new(), Flag::new(), Flag::new());
    relay.submit_in(
        "primary",
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                nested = root.nested  # obtained on connection 1
                w = worker(lambda: root.record(0, 'hold', started={held}, until={release}))
                await appears({held})  # connection 1 has a call in flight
                conn = object.__getattribute__(nested, '____conn__')
                print(nested.value, nested.method(), conn.number, root.takes(nested))
                touch({done})
                await w
                print(numbers())
                "#,
                held = held.py(),
                release = release.py(),
                done = done.py()
            ),
        ),
    );
    // The proxy's requests returned while its own connection was busy.
    done.wait();
    release.raise();
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(
        result["output"], "7 nested method 1 True\n[1, 2]\n",
        "{result}"
    );
    assert_eq!(ids_used(&relay, "primary"), [1, 2]);
}

#[test]
fn a_callbacks_own_requests_are_checked() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            seen = []
            def fn(x):
                try:
                    root._private
                except Exception as e:
                    seen.append(f"{type(e).__name__}: {str(e).splitlines()[0]}")
                return x
            print(root.call(fn, 1))
            print(seen)
            "#,
        ),
    );
    assert!(
        out.starts_with("1\n") && out.contains("Refused") && out.contains("not a public name"),
        "{out}"
    );
}

#[test]
fn nested_callbacks_complete_with_every_connection_occupied() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_pool_in(
            "primary",
            r#"
            barrier = threading.Barrier(4, timeout=20)
            def fn(i):
                barrier.wait()  # every connection holds a call inside a callback
                return root.noted()
            ws = [worker(lambda i=i: root.call(fn, i)) for i in range(4)]
            print(sorted(await asyncio.gather(*ws)), numbers(), root.calls)
            "#,
        ),
    );
    assert_eq!(out, "[1, 2, 3, 4] [1, 2, 3, 4] 4\n");
    assert_eq!(ids_used(&relay, "primary"), [1, 2, 3, 4]);
}

/// Python flags as a list literal.
fn flag_list(flags: &[Flag]) -> String {
    let items: Vec<String> = flags.iter().map(Flag::py).collect();
    format!("[{}]", items.join(", "))
}

#[test]
fn a_blocked_call_delays_neither_another_kernel_nor_a_worker() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    let (started, worker_done) = (Flag::new(), Flag::new());
    // A's thread blocks for a full minute, as the acceptance says; its worker's call -- made
    // once A's call is running on the host -- and B's call return meanwhile, each within its
    // own duration.
    relay.submit_in(
        "primary",
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                def once_blocked():
                    while not os.path.exists({started}):
                        time.sleep(0.01)
                    label = root.record(0, 'w')
                    touch({done})
                    return label
                w = worker(once_blocked)
                print(root.record(60, 'a', started={started}))
                print(await w)
                "#,
                done = worker_done.py(),
                started = started.py()
            ),
        ),
    );
    started.wait();
    let asked = Instant::now();
    assert_eq!(
        relay.output_in("b", 1, &with_root_in("b", "print(root.record(0, 'b'))")),
        "b\n"
    );
    worker_done.wait();
    let took = asked.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "B's call and A's worker call took {took:?} while A was blocked"
    );
    let result = relay.result_from("primary", 1, Duration::from_secs(90));
    assert_eq!(result["output"], "a\nw\n", "{result}");
    let recorded = intervals(&mut relay, "b", 2);
    let a = interval(&recorded, "a");
    assert!(a.end - a.start >= 60.0, "{recorded:?}");
    for label in ["b", "w"] {
        let other = interval(&recorded, label);
        assert!(
            a.start <= other.start && other.end <= a.end,
            "{label} did not run inside A's call: {recorded:?}"
        );
    }
}

// ---------------------------------------------------------------------------- serialize

#[test]
fn the_serialize_lock_is_reentrant_and_fifo() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("binding.py");
    std::fs::write(&source, BINDING_SOURCE).expect("write the binding program");
    let driver = py(r#"
        import json, sys, threading, time
        source = sys.argv[1]
        ns = {"__name__": "outrig_binding"}
        exec(compile(open(source).read(), source, "exec"), ns)
        turn = ns["Turn"]()
        order = []
        turn.__enter__()
        turn.__enter__()  # re-entrant for the holder: nobody is queued
        queued_by_holder = len(turn._waiters)
        turn.__exit__(None, None, None)
        def waiter(i):
            with turn:
                order.append(i)
        threads = []
        for i in range(4):
            t = threading.Thread(target=waiter, args=(i,))
            t.start()
            threads.append(t)
            deadline = time.monotonic() + 20
            while len(turn._waiters) < i + 1:  # queued in arrival order before the next starts
                if time.monotonic() > deadline:
                    raise TimeoutError("a waiter never queued")
                time.sleep(0.001)
        turn.__exit__(None, None, None)  # depth 1 -> 0: the head of the queue takes it
        for t in threads:
            t.join(20)
        print(json.dumps({"queued_by_holder": queued_by_holder, "order": order,
                          "waiting": len(turn._waiters), "owner": turn._owner}))
    "#);
    let output = Command::new(python())
        .args(["-I", "-c", &driver])
        .arg(&source)
        .output()
        .expect("the payload's Python runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("a JSON report");
    assert_eq!(report["queued_by_holder"], 0, "{report}");
    assert_eq!(report["order"], json!([0, 1, 2, 3]), "{report}");
    assert_eq!(report["waiting"], 0, "{report}");
    assert!(report["owner"].is_null(), "{report}");
}

/// Wait until `agent`'s traffic toward `fx` has grown past `before`: its request has reached the
/// binding, where -- under `serialize` -- it waits for the lock.
fn frame_reached_binding(relay: &Relay, agent: &str, before: usize) {
    let stats = Arc::clone(&relay.stats);
    eventually(
        || (stats.between(agent, "fx").to_binding.lines() > before).then_some(()),
        || format!("{agent}'s request reaching the binding"),
    );
}

#[test]
fn a_serialized_binding_runs_one_call_at_a_time() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.open("b");
    // Both kernels resolve the root first, so the recorded calls are the only requests.
    relay.output(1, &with_root("root.public"));
    relay.output_in("b", 1, &with_root_in("b", "root.public"));
    let (started, release) = (Flag::new(), Flag::new());
    relay.submit_in(
        "primary",
        2,
        &with_root(&format!(
            "print(root.record(0, 'a', started={}, until={}))",
            started.py(),
            release.py()
        )),
    );
    started.wait();
    let before = relay.stats.between("b", "fx").to_binding.lines();
    relay.submit_in("b", 2, &with_root_in("b", "print(root.record(0, 'b'))"));
    frame_reached_binding(&relay, "b", before);
    release.raise();
    assert_eq!(relay.result_from("primary", 2, TIMEOUT)["output"], "a\n");
    assert_eq!(relay.result_from("b", 2, TIMEOUT)["output"], "b\n");
    let recorded = intervals(&mut relay, "primary", 3);
    let (a, b) = (interval(&recorded, "a"), interval(&recorded, "b"));
    assert!(
        b.start >= a.end,
        "the kernels' calls overlapped: {recorded:?}"
    );

    // Two workers of one kernel, likewise.
    let flags: Vec<Flag> = (0..3).map(|_| Flag::new()).collect();
    let (s1, f1, w2_sent) = (&flags[0], &flags[1], &flags[2]);
    let before = relay.stats.between("primary", "fx").to_binding.lines();
    relay.submit_in(
        "primary",
        4,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                w1 = worker(lambda: root.record(0, 'w1', started={s1}, until={f1}))
                await appears({s1})
                w2 = worker(lambda: root.record(0, 'w2'))
                touch({sent})
                print(await asyncio.gather(w1, w2))
                "#,
                s1 = s1.py(),
                f1 = f1.py(),
                sent = w2_sent.py()
            ),
        ),
    );
    w2_sent.wait();
    frame_reached_binding(&relay, "primary", before + 1);
    f1.raise();
    assert_eq!(
        relay.result_from("primary", 4, TIMEOUT)["output"],
        "['w1', 'w2']\n"
    );
    let recorded = intervals(&mut relay, "primary", 5);
    let (w1, w2) = (interval(&recorded, "w1"), interval(&recorded, "w2"));
    assert!(
        w2.start >= w1.end,
        "the workers' calls overlapped: {recorded:?}"
    );
}

#[test]
fn a_callbacks_request_runs_under_serialize_while_another_kernel_waits() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.open("b");
    relay.output(1, &with_root("root.public"));
    relay.output_in("b", 1, &with_root_in("b", "root.public"));
    let (in_callback, go) = (Flag::new(), Flag::new());
    // A's call runs a callback, which makes a hosted call of its own while B's call waits for
    // the lock: the nested call runs at once, on the thread holding it.
    relay.submit_in(
        "primary",
        2,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                def fn():
                    touch({in_callback})
                    deadline = time.monotonic() + 20
                    while not os.path.exists({go}):
                        if time.monotonic() > deadline:
                            raise TimeoutError('never released')
                        time.sleep(0.01)
                    return root.noted()
                print(root.call_recorded(fn, 'a'))
                "#,
                in_callback = in_callback.py(),
                go = go.py()
            ),
        ),
    );
    in_callback.wait();
    let before = relay.stats.between("b", "fx").to_binding.lines();
    relay.submit_in("b", 2, &with_root_in("b", "print(root.record(0, 'b'))"));
    frame_reached_binding(&relay, "b", before);
    go.raise();
    assert_eq!(relay.result_from("primary", 2, TIMEOUT)["output"], "1\n");
    assert_eq!(relay.result_from("b", 2, TIMEOUT)["output"], "b\n");
    let recorded = intervals(&mut relay, "primary", 3);
    let (a, b) = (interval(&recorded, "a"), interval(&recorded, "b"));
    assert!(b.start >= a.end, "B's call ran inside A's: {recorded:?}");
}

// ---------------------------------------------------------------------------- interrupts

/// Interrupt -- or cancel -- `agent` while its thread waits on a hosted call: the call raises at
/// once, the next call runs on another connection, and the interrupted connection is free again
/// once its late reply has arrived, which reaches nobody.
fn an_interrupted_call_leaves_its_connection_usable(agent: &str, cancel: bool) {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    if agent != "primary" {
        relay.open(agent);
    }
    let (started, release) = (Flag::new(), Flag::new());
    relay.submit_in(
        agent,
        1,
        &with_root_in(
            agent,
            &format!(
                "print(root.record(10, 'slow', started={}, until={}))",
                started.py(),
                release.py()
            ),
        ),
    );
    started.wait();
    if cancel {
        relay.cancel(agent, 1);
    } else {
        relay.interrupt(agent, 1, false);
    }
    let result = relay.result_from(agent, 1, TIMEOUT);
    assert_eq!(result["status"], "error", "{result}");
    let (raised, verb) = if cancel {
        ("CancelledError", "cancelled")
    } else {
        ("KeyboardInterrupt", "interrupted")
    };
    assert_eq!(result["raised"], raised, "{result}");
    let error = result["error"].as_str().expect("an error");
    assert!(
        error.contains(&format!(
            "{verb} by outrig while waiting for binding 'fx' to answer: the call's outcome on \
             the host is unknown"
        )),
        "{error}"
    );
    assert!(!release.exists());
    // The next call returns its own result, on a second connection; the first still has the
    // interrupted call's reply to come.
    let out = relay.output_in(
        agent,
        2,
        &with_pool_in(agent, "print(root.public, pending(), numbers())"),
    );
    assert_eq!(out, "public value [1] [1, 2]\n");
    assert_eq!(ids_used(&relay, agent), [1, 2]);
    release.raise();
    // Once the late reply has arrived, the next call tidies it away -- nobody receives `slow`
    // -- and the connection is free again.
    let out = relay.output_in(
        agent,
        3,
        &with_pool_in(
            agent,
            r#"
            stale = next(c for c in pool.connections if c._request_callbacks)
            deadline = time.monotonic() + 20
            while not stale._channel.poll(0):
                if time.monotonic() > deadline:
                    raise TimeoutError('the late reply never came')
                await asyncio.sleep(0.01)
            print(root.method(1), pending(), numbers())
            "#,
        ),
    );
    assert_eq!(out, "2 [] [1, 2]\n");
    assert_eq!(ids_used(&relay, agent), [1, 2]);
    let recorded = intervals(&mut relay, agent, 4);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].label, "slow");
}

#[test]
fn an_interrupted_call_leaves_its_connection_usable_on_a_child() {
    an_interrupted_call_leaves_its_connection_usable("b", false);
}

#[test]
fn an_interrupted_call_leaves_its_connection_usable_on_the_primary() {
    an_interrupted_call_leaves_its_connection_usable("primary", false);
}

#[test]
fn a_cancelled_call_leaves_its_connection_usable() {
    an_interrupted_call_leaves_its_connection_usable("b", true);
}

#[test]
fn a_call_woken_from_the_connection_wait_was_never_sent() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    let starts: Vec<Flag> = (0..4).map(|_| Flag::new()).collect();
    let releases: Vec<Flag> = (0..4).map(|_| Flag::new()).collect();
    let four = Flag::new();
    relay.submit_in(
        "b",
        1,
        &with_pool_in(
            "b",
            &format!(
                r#"
                S, F = {starts}, {releases}
                ws = [worker(lambda i=i: root.record(0, f'w{{i}}', started=S[i], until=F[i]))
                      for i in range(4)]
                for s in S:
                    await appears(s)
                touch({four})
                print(root.noted())  # B's own thread waits for a free connection
                "#,
                starts = flag_list(&starts),
                releases = flag_list(&releases),
                four = four.py()
            ),
        ),
    );
    four.wait();
    let waiting = with_root(
        r#"
        b = __main__._kernels['b']
        w = __main__._waits.get(b.thread.ident)
        print(w is not None and w.conn is None and w.waiting)
        "#,
    );
    eventually(
        || (relay.output(1, &waiting) == "True\n").then_some(()),
        || "B's thread waiting for a connection".to_string(),
    );
    let before = relay.stats.between("b", "fx").to_binding.lines();
    relay.interrupt("b", 1, false);
    let result = relay.result_from("b", 1, TIMEOUT);
    assert_eq!(result["raised"], "KeyboardInterrupt", "{result}");
    let error = result["error"].as_str().expect("an error");
    assert!(
        error.contains(
            "interrupted by outrig while waiting for a free connection to binding 'fx': the call \
             was never sent"
        ),
        "{error}"
    );
    assert_eq!(relay.stats.between("b", "fx").to_binding.lines(), before);
    assert_eq!(relay.output(2, &with_root("print(root.calls)")), "0\n");
    for release in &releases {
        release.raise();
    }
    eventually(
        || (intervals(&mut relay, "primary", 3).len() == 4).then_some(()),
        || "the four workers' calls ending".to_string(),
    );
    assert_eq!(relay.output(4, &with_root("print(root.calls)")), "0\n");
    assert_eq!(relay.stats.between("b", "fx").to_binding.lines(), before);
}

// ---------------------------------------------------------------------------- the shared table

#[test]
fn a_closed_connection_takes_only_its_own_entries_from_the_kernels_table() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let flags: Vec<Flag> = (0..5).map(|_| Flag::new()).collect();
    let (s1, f1, s2, f2, closed) = (&flags[0], &flags[1], &flags[2], &flags[3], &flags[4]);
    relay.submit_in(
        "primary",
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                x1 = root.fresh('x')  # handed out on connection 1
                y1 = root.fresh('y')  # likewise
                w1 = worker(lambda: root.record(0, 'w1', started={s1}, until={f1}))
                await appears({s1})  # connection 1 is busy
                x2 = root.again('x')  # the same object, handed out on connection 2
                root.let_go('x')
                root.let_go('y')  # the table alone holds both now
                w2 = worker(lambda: x2.record(label='w2', started={s2}, until={f2}))
                await appears({s2})  # a call through connection 2's proxy is in flight
                touch({f1})
                await w1
                c1 = next(c for c in pool.connections if c.number == 1)
                c1.close_with('closed by the test')
                touch({closed})
                print(await w2)
                print(x2.name)
                try:
                    x1.name
                except EOFError as e:
                    print('x1', str(e).splitlines()[0])
                __main__.keep = (x1, x2, y1)  # no finalizer releases anything mid-test
                "#,
                s1 = s1.py(),
                f1 = f1.py(),
                s2 = s2.py(),
                f2 = f2.py(),
                closed = closed.py()
            ),
        ),
    );
    closed.wait();
    let stats = Arc::clone(&relay.stats);
    eventually(
        || (stats.between("primary", "fx").closed_to_binding() == 1).then_some(()),
        || "the close notice reaching the binding".to_string(),
    );
    f2.raise();
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(
        result["output"], "w2\nx\nx1 closed by the test\n",
        "{result}"
    );
    // The binding dropped connection 1's references and no others: `y` is gone, `x` stays.
    let alive = with_root("print(root.alive('x'), root.alive('y'))");
    eventually(
        || (relay.output(2, &alive) == "True False\n").then_some(()),
        || "connection 1's references to be dropped".to_string(),
    );
    let recorded = intervals(&mut relay, "primary", 3);
    assert_eq!(interval(&recorded, "w2").thread, "serve-primary-2");
}

#[test]
fn a_released_pool_leaves_its_worker_calls_unknown_and_its_table_gone() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("c");
    let flags: Vec<Flag> = (0..5).map(|_| Flag::new()).collect();
    let (s1, f1, s2, f2, both) = (&flags[0], &flags[1], &flags[2], &flags[3], &flags[4]);
    assert_eq!(relay.output(1, &with_root("print(root.method(1))")), "2\n");
    relay.submit_in(
        "c",
        1,
        &with_pool_in(
            "c",
            &format!(
                r#"
                __main__.keep_z = root.fresh('z')
                root.let_go('z')  # the kernel's table alone holds it
                w1 = worker(lambda: root.record(0, 'w1', started={s1}, until={f1}))
                w2 = worker(lambda: root.record(0, 'w2', started={s2}, until={f2}))
                await appears({s1})
                await appears({s2})
                touch({both})
                res = await asyncio.gather(w1, w2, return_exceptions=True)
                print([f"{{type(r).__name__}}: {{r}}" for r in res])
                print('fx' in K._hosted)
                "#,
                s1 = s1.py(),
                f1 = f1.py(),
                s2 = s2.py(),
                f2 = f2.py(),
                both = both.py()
            ),
        ),
    );
    both.wait();
    // The release, as `0003-26` will make it, from another thread.
    relay.output(2, "import __main__\n__main__._kernels['c'].close_hosted()");
    let result = relay.result_from("c", 1, TIMEOUT);
    let unknown =
        "EOFError: the kernel's connections are closing: the call's outcome on the host is unknown";
    assert_eq!(
        result["output"],
        format!("[\"{unknown}\", \"{unknown}\"]\nFalse\n"),
        "{result}"
    );
    assert_eq!(relay.stats.between("c", "fx").closed_to_binding(), 2);
    let replies_before = relay.stats.between("c", "fx").to_interpreter.lines();
    assert_eq!(relay.output(3, &with_root("print(root.method(2))")), "3\n");
    f1.raise();
    f2.raise();
    // The binding finishes both calls -- their intervals end -- and their replies cross to
    // nobody: the connections are closed, and the kernel's table in the binding is gone with
    // the object only it held.
    eventually(
        || {
            let recorded = intervals(&mut relay, "primary", 4);
            (["w1", "w2"]
                .iter()
                .all(|label| recorded.iter().any(|i| &i.label == label)))
            .then_some(())
        },
        || "both worker calls to finish on the host".to_string(),
    );
    eventually(
        || {
            relay
                .binding_stderr()
                .contains(
                    "agent 'c': its last connection has ended, and its object table is dropped",
                )
                .then_some(())
        },
        || {
            format!(
                "the table to be dropped; binding stderr: {}",
                relay.binding_stderr()
            )
        },
    );
    let alive = with_root("print(root.alive('z'))");
    eventually(
        || (relay.output(5, &alive) == "False\n").then_some(()),
        || "z to be dropped".to_string(),
    );
    assert_eq!(
        relay.stats.between("c", "fx").to_interpreter.lines(),
        replies_before
    );
    // The kernel itself carries on, with a new pool.
    assert_eq!(
        relay.output_in(
            "c",
            2,
            &with_pool_in("c", "print(root.method(3), numbers())")
        ),
        "4 [3]\n"
    );
}

#[test]
fn reference_counts_span_the_kernels_connections() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (held, release) = (Flag::new(), Flag::new());
    let out = relay.output(
        1,
        &with_root(&format!(
            "{}\n{}\n{}",
            py(POOL),
            py(RAW),
            py(&format!(
                r#"
                x1 = root.fresh('x')
                x1b = root.again('x')  # connection 1 handed x out twice
                w = worker(lambda: root.record(0, 'hold', started={held}, until={release}))
                await appears({held})
                x2 = root.again('x')  # once on connection 2
                root.let_go('x')
                touch({release})
                await w
                c1, c2 = sorted(pool.connections, key=lambda c: c.number)
                DEL = consts.HANDLE_DEL
                attempt('inflated', lambda: raw_on(c1, DEL, (T, ((L, idp(x1)), (V, 3)))))
                print('after inflated', root.alive('x'))
                attempt('first', lambda: raw_on(c1, DEL, (T, ((L, idp(x1)), (V, 2)))))
                print('after first', root.alive('x'), x2.name)
                attempt('second', lambda: raw_on(c2, DEL, (T, ((L, idp(x2)), (V, 1)))))
                print('after second', root.alive('x'))
                __main__.keep = (x1, x1b, x2)
                "#,
                held = held.py(),
                release = release.py()
            ))
        )),
    );
    refused(
        &out,
        "inflated",
        "a release of 3 references to an object this connection handed out 2 times",
    );
    assert!(out.contains("after inflated True\n"), "{out}");
    assert!(out.contains("first -> None\n"), "{out}");
    assert!(out.contains("after first True x\n"), "{out}");
    assert!(out.contains("second -> None\n"), "{out}");
    assert!(out.contains("after second False\n"), "{out}");
}

// ---------------------------------------------------------------------------- lifetimes under the pool

#[test]
fn an_interrupted_calls_callables_are_released_when_its_reply_is_dropped() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    let (started, release) = (Flag::new(), Flag::new());
    // B's call carries a callable the host holds while it blocks. The callable lives in a
    // function's scope, so once the call has raised nothing but the connection's table of
    // callables marked for the request can keep it alive.
    relay.submit_in(
        "b",
        1,
        &with_pool_in(
            "b",
            &format!(
                r#"
                import weakref
                def run():
                    def fn():
                        return 1
                    __main__.held = weakref.ref(fn)
                    root.hold_callable(fn, started={started}, until={release})
                run()
                "#,
                started = started.py(),
                release = release.py()
            ),
        ),
    );
    started.wait();
    relay.interrupt("b", 1, false);
    let result = relay.result_from("b", 1, TIMEOUT);
    assert_eq!(result["raised"], "KeyboardInterrupt", "{result}");
    release.raise();
    // Once the late reply has arrived, the next call drops it -- and with it the hold on the
    // callable, which the host revoked before replying.
    let out = relay.output_in(
        "b",
        2,
        &with_pool_in(
            "b",
            r#"
            import gc
            stale = next(c for c in pool.connections if c._request_callbacks)
            deadline = time.monotonic() + 20
            while not stale._channel.ready:
                if time.monotonic() > deadline:
                    raise TimeoutError('the late reply never came')
                await asyncio.sleep(0.01)
            root.public
            gc.collect()
            print(__main__.held() is None, pending())
            "#,
        ),
    );
    assert_eq!(out, "True []\n");
}

#[test]
fn a_release_queued_on_an_idle_connection_is_sent_by_the_next_call() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (held, release) = (Flag::new(), Flag::new());
    // `y` is a proxy produced on connection 2 while connection 1 was busy, so its release is
    // queued there, where only a call that takes connection 2 can send it.
    let out = relay.output(
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                import gc
                x = root.fresh('x')
                w = worker(lambda: root.record(0, 'hold', started={held}, until={release}))
                await appears({held})
                y = root.again('x')
                root.let_go('x')
                touch({release})
                await w
                print(object.__getattribute__(y, '____conn__').number, numbers())
                del x, y
                gc.collect()
                for _ in range(3):
                    root.public
                print(root.alive('x'))
                "#,
                held = held.py(),
                release = release.py()
            ),
        ),
    );
    assert_eq!(out, "2 [1, 2]\nFalse\n");
}

#[test]
fn a_connection_closed_under_a_callback_holds_nothing_afterwards() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    // The callback closes the connection its call is on. The host's call of it raises; the
    // library swallows that and answers with a new object, which the host cannot send.
    let out = relay.output(
        1,
        &with_pool_in(
            "primary",
            r#"
            def fn():
                mine = next(c for c in pool.connections if c.owner == threading.get_ident())
                mine.close_with('closed by the test')
            try:
                root.call_then_fresh(fn, 'q')
            except EOFError as e:
                print('raised', str(e).splitlines()[0])
            "#,
        ),
    );
    assert_eq!(out, "raised closed by the test\n");
    // Once the host has dropped the reply, the object it carried is held by nothing but the
    // fixture, and letting that go frees it: the closed connection kept no reference.
    let probe = with_root(
        r#"
        try:
            root.let_go('q')
        except Exception:
            print('not yet')
        else:
            print(root.alive('q'))
        "#,
    );
    eventually(
        || (relay.output(2, &probe) == "False\n").then_some(()),
        || "the object the unsent reply carried to be freed".to_string(),
    );
}

#[test]
fn a_callable_a_method_returns_is_the_one_passed() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    // The host sends the callable back as a reference into the connection's table of callables
    // marked for the request, which must still hold it; afterwards the table is empty.
    let out = relay.output(
        1,
        &with_root(
            r#"
            f = lambda: 1
            print(root.echo(f) is f, root.echo((f, 2))[0] is f)
            conn = object.__getattribute__(root, '____conn__')
            print(len(conn._local_objects._dict))
            "#,
        ),
    );
    assert_eq!(out, "True True\n0\n");
}

#[test]
fn releases_queued_on_several_connections_are_all_sent_under_steady_traffic() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (held, release) = (Flag::new(), Flag::new());
    // Releases are queued on connections 1 and 2. Every call then takes the connection with
    // queued releases that waited longest, so a factory's result discarded on each call --
    // which requeues a release on whichever connection the call took -- does not keep one
    // connection from ever sending its own.
    let out = relay.output(
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                import gc
                x = root.fresh('x')
                w = worker(lambda: root.record(0, 'hold', started={held}, until={release}))
                await appears({held})
                y = root.again('x')
                root.let_go('x')
                touch({release})
                await w
                del x, y
                gc.collect()
                for _ in range(100):
                    n = root.nested
                    del n
                    gc.collect()
                print(root.alive('x'), numbers())
                "#,
                held = held.py(),
                release = release.py()
            ),
        ),
    );
    assert_eq!(out, "False [1, 2]\n");
}

// ---------------------------------------------------------------------------- the binding as a service client

/// Python that counts the requests each operation makes, by handler, on the container side: it
/// wraps the connection class's `_send`, so a release flushed before the next request shows as
/// its own line. Each operation is measured with the connection quiet before it and its proxies
/// released after it, and reported as `{"requests": {handler: count}, "dels": count}`.
const COUNTING: &str = r#"
    import collections, gc, json
    from rpyc.core import consts
    H = type(object.__getattribute__(root, '____conn__'))
    names = {v: n for n, v in vars(consts).items() if n.startswith('HANDLE_')}
    counts = collections.Counter()
    original = H._send
    def counting(self, msg, seq, args):
        if msg == consts.MSG_REQUEST:
            counts[names.get(args[0], str(args[0]))] += 1
        return original(self, msg, seq, args)
    H._send = counting
    report = {}
    def measure(label, fn):
        gc.collect()
        root.public  # nothing from before is pending
        counts.clear()
        result = fn()
        ops = dict(counts)
        counts.clear()
        del result
        gc.collect()
        root.public  # the operation's proxies release before this request
        report[label] = {"requests": ops, "dels": counts.get('HANDLE_DEL', 0)}
"#;

#[test]
fn each_operations_request_count_is_recorded() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let before = relay.stats.between("primary", "fx").to_binding.lines();
    let out = relay.output(
        1,
        &with_root(&format!(
            "{}\n{}",
            py(COUNTING),
            py(r#"
                kept = {}
                measure('ask', lambda: kept.setdefault('ticket', root.ask('how long?')))
                ticket = kept['ticket']
                measure('poll_none', lambda: root.poll(ticket))
                measure('poll_answer', lambda: root.poll(ticket))
                def by_value():
                    fields, rows = root.records_by_value(300)
                    return [dict(zip(fields, row)) for row in rows]
                measure('records_by_value', by_value)
                measure('proxied_list', lambda: [(r['id'], r['name']) for r in root.list_records(300)])
                measure('update_records',
                        lambda: root.update_records([{'id': i, 'score': 0.0} for i in range(300)]))
                print(json.dumps(report))
            "#)
        )),
    );
    let lines = relay.stats.between("primary", "fx").to_binding.lines() - before;
    let report: Value = serde_json::from_str(out.trim()).expect("a JSON report");
    println!("MEASUREMENT request counts: {report}; rpc lines toward the binding: {lines}");
    // A method call on a proxy is two requests, not one: RPyC's `__getattribute__` fetches the
    // bound method as a proxy of its own, and the call is a `call` on that proxy, which is then
    // released. The `callattr` request, one round trip, is sent only for the special methods
    // Python looks up on the type -- `__iter__`, `__next__`, `__getitem__` -- so iterating a
    // proxied list of 300 dicts and reading two fields of each costs about 900 of them, and
    // a release per proxy as they go.
    let two_requests = json!({"HANDLE_GETATTR": 1, "HANDLE_CALL": 1});
    for op in [
        "ask",
        "poll_none",
        "poll_answer",
        "records_by_value",
        "update_records",
    ] {
        assert_eq!(report[op]["requests"], two_requests, "{op}: {report}");
        assert_eq!(report[op]["dels"], 1, "{op}: {report}");
    }
    let iterated = &report["proxied_list"]["requests"];
    assert_eq!(iterated["HANDLE_GETATTR"], 1, "{report}");
    assert_eq!(iterated["HANDLE_CALL"], 1, "{report}");
    assert!(
        iterated["HANDLE_CALLATTR"].as_u64().expect("callattrs") >= 900,
        "{report}"
    );
    assert!(
        iterated["HANDLE_DEL"].as_u64().expect("dels") >= 300,
        "{report}"
    );
}

/// The measurements of the task's second half. They block a kernel for minutes, so they run by
/// hand -- `cargo test -p outrig --lib python::binding_tests::measurements -- --ignored
/// --nocapture --test-threads=1` -- and the numbers they print go into the task's
/// `## Decisions`. `OUTRIG_MEASURE_SECS` sets the block and the measuring window, 180 by
/// default.
mod measurements {
    use super::*;

    fn secs() -> u64 {
        std::env::var("OUTRIG_MEASURE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180)
    }

    /// Python that runs the service client's operations against `root` for `secs` seconds from
    /// the first call's return -- the first call's own wait is reported apart, since under
    /// `serialize` it waits behind the blocked call -- and prints each operation's count,
    /// median, 99th percentile and maximum latency in milliseconds.
    fn measuring_loop(secs: u64) -> String {
        py(&format!(
            r#"
            import json, time
            def percentile(xs, p):
                xs = sorted(xs)
                return xs[min(len(xs) - 1, int(p * len(xs)))]
            lat = {{}}
            def timed(key, fn):
                t = time.perf_counter()
                r = fn()
                lat.setdefault(key, []).append((time.perf_counter() - t) * 1000)
                return r
            t0 = time.perf_counter()
            ticket = timed('ask', lambda: root.ask('how long?'))
            first_wait_ms = (time.perf_counter() - t0) * 1000
            deadline = time.perf_counter() + {secs}
            rounds = 0
            while time.perf_counter() < deadline:
                rounds += 1
                if rounds > 1:
                    ticket = timed('ask', lambda: root.ask('how long?'))
                timed('poll_none', lambda: root.poll(ticket))
                timed('poll_answer', lambda: root.poll(ticket))
                timed('records_by_value', lambda: root.records_by_value(300))
                timed('update_records',
                      lambda: root.update_records([{{'id': i, 'score': 1.0}} for i in range(300)]))
                if rounds % 10 == 1:
                    timed('proxied_list', lambda: [r['id'] for r in root.list_records(300)])
                time.sleep(0.01)
            report = {{k: {{'n': len(v), 'median_ms': round(percentile(v, 0.5), 3),
                           'p99_ms': round(percentile(v, 0.99), 3), 'max_ms': round(max(v), 3)}}
                      for k, v in lat.items()}}
            report['first_wait_ms'] = round(first_wait_ms, 3)
            report['rounds'] = rounds
            print(json.dumps(report))
            "#
        ))
    }

    /// `agent`'s result `id`, sampling `sample` every 30 seconds meanwhile.
    fn result_sampling(
        relay: &mut Relay,
        agent: &str,
        id: u64,
        within: Duration,
        mut sample: impl FnMut(&mut Relay),
    ) -> Value {
        let deadline = Instant::now() + within;
        loop {
            if let Some(result) = relay.try_result_from(agent, id, Duration::from_secs(30)) {
                return result;
            }
            assert!(
                Instant::now() < deadline,
                "no result from {agent}/{id} in {within:?}"
            );
            sample(relay);
        }
    }

    fn wait_minutes_holds_one_connection(serialize: bool) {
        let secs = secs();
        let mut relay = Relay::start();
        if serialize {
            relay.bind_serialized("fx", FACTORY);
        } else {
            relay.bind("fx", FACTORY);
        }
        relay.open("b");
        relay.output(1, &with_root("root.public"));
        relay.output_in("b", 1, &with_root_in("b", "root.public"));
        let rss_before = relay.binding_rss_kib("fx");
        let release = Flag::new();
        // A waits for minutes through a worker, so its loop keeps answering `inv`.
        relay.submit_in(
            "primary",
            2,
            &with_pool_in(
                "primary",
                &format!(
                    "print(await loop.run_in_executor(None, lambda: root.wait_for_answer(until={})))",
                    release.py()
                ),
            ),
        );
        let started = Instant::now();
        relay.submit_in("b", 2, &with_root_in("b", &measuring_loop(secs)));
        let mut rss_during = Vec::new();
        let mut inv_id = 100;
        let mut released = false;
        let result = result_sampling(
            &mut relay,
            "b",
            2,
            Duration::from_secs(3 * secs + 120),
            |relay| {
                inv_id += 1;
                assert!(relay.inv("primary", inv_id)["globals"].is_array());
                rss_during.push(relay.binding_rss_kib("fx"));
                if !released && started.elapsed() >= Duration::from_secs(secs) {
                    release.raise();
                    released = true;
                }
            },
        );
        if !released {
            release.raise();
        }
        assert_eq!(result["status"], "ok", "{result}");
        let a = relay.result_from("primary", 2, TIMEOUT);
        assert_eq!(a["output"], "released by file\n", "{a}");
        relay.output(3, &with_root("import gc\ngc.collect()\nroot.public"));
        relay.output_in(
            "b",
            3,
            &with_root_in("b", "import gc\ngc.collect()\nroot.public"),
        );
        let rss_after = relay.binding_rss_kib("fx");
        println!(
            "MEASUREMENT wait_minutes_holds_one_connection serialize={serialize} secs={secs}: \
             {}\n  binding RSS KiB before={rss_before} during={rss_during:?} after={rss_after}",
            result["output"].as_str().unwrap_or("").trim()
        );
    }

    #[test]
    #[ignore = "blocks a kernel for minutes; run by hand for the record"]
    fn wait_minutes_holds_one_connection_plain() {
        wait_minutes_holds_one_connection(false);
    }

    #[test]
    #[ignore = "blocks a kernel for minutes; run by hand for the record"]
    fn wait_minutes_holds_one_connection_serialized() {
        wait_minutes_holds_one_connection(true);
    }

    fn two_sessions_are_two_clients(serialize: bool) {
        let secs = secs();
        let service = Service::start();
        let bind = Bind {
            factory: "outrig_fixture:service_client",
            env: &[("OUTRIG_FIXTURE_SERVICE", service.socket())],
            serialize,
            ..Bind::default()
        };
        let mut one = Relay::start();
        one.bind_opts("fx", &bind);
        let mut two = Relay::start();
        two.bind_opts("fx", &bind);
        // Each session resolves its root and makes one call, so the measured calls are the
        // only ones.
        one.output(1, &with_root("root.ask('warm')"));
        two.output(1, &with_root("root.ask('warm')"));
        let rss_before = (one.binding_rss_kib("fx"), two.binding_rss_kib("fx"));
        // Session 1 waits for an answer the service has not got; session 2 measures against the
        // same service, then answers.
        one.submit_in(
            "primary",
            2,
            &with_pool_in(
                "primary",
                "print(await loop.run_in_executor(None, lambda: root.wait_for_answer()))",
            ),
        );
        two.submit_in(
            "primary",
            2,
            &with_root(&format!("{}\nroot.answer('done')", measuring_loop(secs))),
        );
        let mut rss_during = Vec::new();
        let result = result_sampling(
            &mut two,
            "primary",
            2,
            Duration::from_secs(3 * secs + 120),
            |two| rss_during.push((one.binding_rss_kib("fx"), two.binding_rss_kib("fx"))),
        );
        assert_eq!(result["status"], "ok", "{result}");
        let waited = one.result_from("primary", 2, TIMEOUT);
        assert_eq!(waited["output"], "done\n", "{waited}");
        let rss_after = (one.binding_rss_kib("fx"), two.binding_rss_kib("fx"));
        println!(
            "MEASUREMENT two_sessions_are_two_clients serialize={serialize} secs={secs}: {}\n  \
             binding RSS KiB (session 1, session 2) before={rss_before:?} during={rss_during:?} \
             after={rss_after:?}",
            result["output"].as_str().unwrap_or("").trim()
        );
    }

    #[test]
    #[ignore = "blocks a session for minutes; run by hand for the record"]
    fn two_sessions_are_two_clients_plain() {
        two_sessions_are_two_clients(false);
    }

    #[test]
    #[ignore = "blocks a session for minutes; run by hand for the record"]
    fn two_sessions_are_two_clients_serialized() {
        two_sessions_are_two_clients(true);
    }
}

/// `runtime.bindings` answers in every kernel, one opened after the session started included,
/// from what the host declared at the start: the binding is stopped, and it still answers.
#[test]
fn runtime_bindings_answers_in_a_kernel_opened_later_without_asking_the_binding() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let fixture = crate::harness::bindings::Brief {
        name: "fx".into(),
        description: "The fixture.".into(),
        type_name: "outrig_fixture.Root".into(),
        serialize: false,
    };
    relay.send(super::host::bindings_message(
        &[fixture],
        [fixture_dir().to_string_lossy().into_owned()],
    ));
    let pid = nix::unistd::Pid::from_raw(relay.binding_pid("fx") as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).expect("stop the binding");
    relay.open("b");
    let ask = "print({n: (b.description, b.type) for n, b in runtime.bindings.items()})";
    let primary = relay.output(1, ask);
    let later = relay.output_in("b", 1, ask);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGCONT).expect("continue it");
    assert_eq!(primary, "{'fx': ('The fixture.', 'outrig_fixture.Root')}\n");
    assert_eq!(later, primary);
}

/// `serialize` in a declaration reaches the binding through the session's own lowering of it:
/// two kernels' calls never overlap on the host under `serialize = true`, and overlap under
/// `serialize = false` and with the key absent.
#[test]
fn serialize_reaches_the_binding_from_its_declaration() {
    use crate::harness::bindings::{Programs, gather, lower};
    for (line, serialized) in [
        ("serialize = true", true),
        ("serialize = false", false),
        ("", false),
    ] {
        let cfg = crate::config::Config::load_from_str(&format!(
            "[bindings.fx]\ndescription = \"d\"\nfactory = {FACTORY:?}\n{line}\n"
        ))
        .expect("the declaration parses");
        let declared = gather(&cfg, &[]).expect("a valid binding");
        let programs = Programs {
            python: python().to_path_buf(),
            rpyc_dir: rpyc_dir().to_path_buf(),
        };
        let here = std::env::current_dir().expect("a working directory");
        let spec =
            lower(&declared[0], fixture_dir(), &programs, &here, &here, None).expect("lowered");
        let mut relay = Relay::start();
        relay.bind_spec("fx", spec);
        relay.open("b");
        relay.output(1, &with_root("root.public"));
        relay.output_in("b", 1, &with_root_in("b", "root.public"));
        let (started, release) = (Flag::new(), Flag::new());
        relay.submit_in(
            "primary",
            2,
            &with_root(&format!(
                "print(root.record(0, 'a', started={}, until={}))",
                started.py(),
                release.py()
            )),
        );
        started.wait();
        let before = relay.stats.between("b", "fx").to_binding.lines();
        relay.submit_in("b", 2, &with_root_in("b", "print(root.record(0, 'b'))"));
        frame_reached_binding(&relay, "b", before);
        if !serialized {
            // Not serialized, `b`'s call runs to its end while `a`'s still waits.
            assert_eq!(
                relay.result_from("b", 2, TIMEOUT)["output"],
                "b\n",
                "{line}"
            );
        }
        release.raise();
        assert_eq!(relay.result_from("primary", 2, TIMEOUT)["output"], "a\n");
        if serialized {
            assert_eq!(relay.result_from("b", 2, TIMEOUT)["output"], "b\n");
        }
        let recorded = intervals(&mut relay, "primary", 3);
        let (a, b) = (interval(&recorded, "a"), interval(&recorded, "b"));
        assert_eq!(a.overlaps(b), !serialized, "{line:?}: {recorded:?}");
    }
}

// ---------------------------------------------------------------------------- the facade

/// Python that binds `K` to `agent`'s kernel and `stub` to its hosted object for binding `fx`,
/// as the session binds one under the binding's name, then runs `body`. Making it opens no
/// connection and sends nothing.
fn with_stub_in(agent: &str, body: &str) -> String {
    format!(
        "import __main__\nAGENT = {agent:?}\nK = __main__._kernels[AGENT]\nstub = K.stub('fx')\n{}",
        py(body)
    )
}

/// [`with_stub_in`] for the primary kernel.
fn with_stub(body: &str) -> String {
    with_stub_in("primary", body)
}

/// [`with_stub_in`], then [`POOL`] -- over the pool the hosted object made -- then `body`.
fn with_stub_pool_in(agent: &str, body: &str) -> String {
    with_stub_in(agent, &format!("{}\n{}", py(POOL), py(body)))
}

/// Python that counts the requests every connection sends, by handler, as [`COUNTING`] does, and
/// binds `measure(label, awaitable)`: it awaits `awaitable` with the connection quiet before, and
/// reports the requests made meanwhile and every release of what it left, as
/// `{"requests": {handler: count}, "dels": count}`. The requests leave out releases and the
/// `inspect` RPyC sends to build a proxy's class for a type it has not seen: bookkeeping, which
/// the inventory does not count as an operation.
const AWAITED_COUNTING: &str = r#"
    import collections, gc, json
    from rpyc.core import consts
    names = {v: n for n, v in vars(consts).items() if n.startswith('HANDLE_')}
    counts = collections.Counter()
    original = __main__._Hosted._send
    def counting(self, msg, seq, args):
        if msg == consts.MSG_REQUEST:
            counts[names.get(args[0], str(args[0]))] += 1
        return original(self, msg, seq, args)
    __main__._Hosted._send = counting
    report = {}
    async def measure(label, awaitable):
        gc.collect()
        await stub.public  # nothing from before is pending
        counts.clear()
        result = await awaitable
        ops = {
            name: n for name, n in counts.items() if name not in ('HANDLE_DEL', 'HANDLE_INSPECT')
        }
        dels = counts.get('HANDLE_DEL', 0)
        counts.clear()
        del result
        gc.collect()
        await stub.public  # what the operation left is released before this request
        report[label] = {"requests": ops, "dels": dels + counts.get('HANDLE_DEL', 0)}
"#;

/// The lines `agent` has sent toward binding `fx`, as the relay counted them.
fn sent(relay: &Relay, agent: &str) -> usize {
    relay.stats.between(agent, "fx").to_binding.lines()
}

/// Reading, calling or indexing a hosted object builds a path and sends nothing, and the path
/// prints a local repr, as an execution's trailing expression too; `help` sends nothing. `await`
/// then gives the fixture's values.
#[test]
fn a_path_crosses_nothing_until_it_is_awaited() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            value = stub.nested.value
            call = stub.method(2, y=3)
            item = stub.sequence[1]
            print(repr(value))
            print(repr(call))
            print(repr(item), type(stub).__name__, stub._binding, stub._netref)
            help(call)
            "#,
        ),
    );
    assert_eq!(sent(&relay, "primary"), 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "<hosted fx.nested.value: unresolved, await it>");
    assert_eq!(lines[1], "<hosted fx.method(...): unresolved, await it>");
    assert_eq!(
        lines[2],
        "<hosted fx.sequence[1]: unresolved, await it> HostedObject fx None"
    );
    assert!(out.contains("Help on HostedObject"), "{out}");
    assert!(
        out.contains("This is a path: nothing has been asked of the host yet."),
        "{out}"
    );
    assert_eq!(
        relay.output(2, "stub.nested"),
        "<hosted fx.nested: unresolved, await it>\n"
    );
    assert_eq!(sent(&relay, "primary"), 0);
    assert_eq!(
        relay.output(3, "print(await value, await call, await item)"),
        "7 5 20\n"
    );
}

/// Awaiting a path sends a request per step, as RPyC's own proxies send them: three `getattr` for
/// `a.b.c`, and for a method call a `getattr`, a `call`, and the bound method's release after.
/// The host's own forms of the local answers go as RPyC's dedicated requests.
#[test]
fn an_awaited_path_makes_a_request_per_step() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(&format!(
            "{}\n{}",
            py(AWAITED_COUNTING),
            py(r#"
                await measure('chain', stub.a.b.c)
                await measure('method', stub.method(1))
                await measure('repr', stub.__repr__())
                await measure('len', stub.sequence.__len__())
                await measure('close', stub.close())
                await measure('send', stub.send(1))
                print(await stub.__repr__())
                print(await stub.sequence.__len__(), [x async for x in stub.received])
                print(json.dumps(report))
            "#)
        )),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[0].starts_with("<outrig_fixture.Root object at "),
        "{out}"
    );
    assert_eq!(lines[1], "3 ['close', ('send', 1)]");
    let report: Value = serde_json::from_str(lines[2]).expect("a JSON report");
    assert_eq!(
        report["chain"]["requests"],
        json!({"HANDLE_GETATTR": 3}),
        "{report}"
    );
    let method = json!({"HANDLE_GETATTR": 1, "HANDLE_CALL": 1});
    for op in ["method", "close", "send"] {
        assert_eq!(report[op]["requests"], method, "{op}: {report}");
        assert_eq!(report[op]["dels"], 1, "{op}: {report}");
    }
    assert_eq!(
        report["repr"]["requests"],
        json!({"HANDLE_REPR": 1}),
        "{report}"
    );
    assert_eq!(
        report["len"]["requests"],
        json!({"HANDLE_GETATTR": 2, "HANDLE_CALL": 1}),
        "{report}"
    );
}

/// What `await` gives for an object: a hosted object, which chains and awaits the same way and
/// awaits as itself with no request; a tuple with hosted objects in it, the same object for the
/// same proxy; and, with its library imported here before it first arrives, its own class for
/// `isinstance`. The inventory names the host's type.
#[test]
fn a_resolved_proxy_chains_and_awaits_as_itself() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(&format!(
            "import sys\nsys.path.append({dir:?})\nimport outrig_fixture\n{counting}\n{body}",
            dir = fixture_dir().to_str().expect("a UTF-8 path"),
            counting = py(AWAITED_COUNTING),
            body = py(r#"
                nested = await stub.nested
                print(repr(nested), type(nested).__name__)
                counts.clear()
                again = await nested
                print(again is nested, dict(counts))
                print(await nested.value, await nested.method())
                pair = await stub.pair()
                print(type(pair).__name__, len(pair), pair[0] is nested, pair[1])
                print(isinstance(nested, outrig_fixture.Nested), nested.__class__.__name__)
                print([n for n in ('_resolve', '_set', 'method') if n in dir(nested)])
                print(runtime.names()['nested'], runtime.names()['stub'])
            "#),
        )),
    );
    assert_eq!(
        out,
        "<hosted outrig_fixture.Nested from fx> HostedObject\n\
         True {}\n\
         7 nested method\n\
         tuple 2 True 1\n\
         True Nested\n\
         ['_resolve', '_set', 'method']\n\
         HostedObject[outrig_fixture.Nested] HostedObject\n"
    );
}

/// A hosted exception is raised where the call is awaited, as the library's own class once its
/// module imports here, with its public attributes, and not as the worker's frames.
#[test]
fn a_hosted_exception_is_raised_typed_where_the_call_is_awaited() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(&format!(
            "import sys\nsys.path.append({dir:?})\nimport outrig_fixture\n{body}",
            dir = fixture_dir().to_str().expect("a UTF-8 path"),
            body = py(r#"
                try:
                    await stub.raise_fixture()
                except outrig_fixture.FixtureError as e:
                    names, tb = [], e.__traceback__
                    while tb is not None:
                        names.append(tb.tb_frame.f_code.co_name)
                        tb = tb.tb_next
                    print(e.args, e.status, [n for n in ('_replay', 'syncreq') if n in names])
            "#),
        )),
    );
    assert_eq!(out, "('fixture failed',) 3 []\n");
}

/// A hosted object passes as an argument whether it is resolved or a path, which the worker
/// resolves first -- inside a list or as a dict key too -- and the host's method runs once for
/// each call. A resolved one whose host type is `list` passes as itself, not as a list to copy.
#[test]
fn a_hosted_object_passes_as_an_argument_resolved_or_not() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            before = await stub.taken
            print(await stub.takes(await stub.nested), await stub.takes(stub.nested))
            print(await stub.taken - before)
            print(await stub.shape([stub.nested, {'k': stub.nested}]))
            print(await stub.shape({stub.nested: 1}))
            records = await stub.list_records(2)
            print(await stub.takes(records), await stub.taken - before)
            "#,
        ),
    );
    assert_eq!(
        out,
        "True True\n2\nlist[Nested, {str: Nested}]\n{Nested: int}\nFalse 3\n"
    );
}

/// `async for` makes `__iter__` and then a `__next__` per item -- and one more for the end --
/// as the host counts them. What would need the host's answer without an `await` raises, naming
/// the spelling that asks for it; `hash` is the object's identity.
#[test]
fn iteration_is_async_and_what_needs_the_host_raises() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            print([x async for x in stub.sequence])
            print(await stub.sequence.iters, await stub.sequence.nexts)
            def attempt(label, fn):
                try:
                    fn()
                except TypeError as e:
                    print(label, '->', e)
            def with_block():
                with stub.manager:
                    pass
            attempt('for', lambda: [x for x in stub.sequence])
            attempt('len', lambda: len(stub.sequence))
            attempt('if', lambda: bool(stub.sequence))
            attempt('in', lambda: 10 in stub.sequence)
            attempt('==', lambda: stub.sequence == [10, 20, 30])
            attempt('with', with_block)
            attempt('+', lambda: stub.sequence + [1])
            print(hash(stub) == object.__hash__(stub), {stub: 1}[stub], callable(stub.public))
            "#,
        ),
    );
    let needs = "of a hosted object needs the host's answer";
    assert_eq!(
        out,
        format!(
            "[10, 20, 30]\n\
             1 4\n\
             for -> __iter__ {needs}: iterate it with `async for item in x`\n\
             len -> __len__ {needs}: `await x.__len__()`\n\
             if -> __bool__ {needs}: `await x.__bool__()`, or `await x.__len__()` for a container\n\
             in -> __contains__ {needs}: `await x.__contains__(item)`\n\
             == -> __eq__ {needs}: `await x.__eq__(other)`; `is` compares the proxies here\n\
             with -> __enter__ {needs}: enter it with `async with x:`\n\
             + -> __add__ {needs}: `await x.__add__(other)`\n\
             True 1 True\n"
        )
    );
}

/// Assignment and deletion are explicit and awaited, each one request; the statements raise
/// with the spelling.
#[test]
fn assignment_and_deletion_are_explicit() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(&format!(
            "{}\n{}",
            py(AWAITED_COUNTING),
            py(r#"
                await measure('set', stub._set('writable', 'after'))
                print(await stub.writable)
                for label, fn in (('assign', lambda: setattr(stub, 'writable', 1)),
                                  ('delete', lambda: delattr(stub, 'writable'))):
                    try:
                        fn()
                    except TypeError as e:
                        print(label, '->', e)
                await measure('delete', stub._delete('writable'))
                try:
                    await stub.writable
                except AttributeError as e:
                    print('deleted:', type(e).__name__)
                print(json.dumps(report))
            "#)
        )),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[..4],
        [
            "after",
            "assign -> an assignment cannot be awaited: `await x._set('writable', value)` \
             assigns on the host",
            "delete -> a deletion cannot be awaited: `await x._delete('writable')` deletes on \
             the host",
            "deleted: AttributeError",
        ]
    );
    let report: Value = serde_json::from_str(lines[4]).expect("a JSON report");
    assert_eq!(
        report["set"]["requests"],
        json!({"HANDLE_SETATTR": 1}),
        "{report}"
    );
    assert_eq!(
        report["delete"]["requests"],
        json!({"HANDLE_DELATTR": 1}),
        "{report}"
    );
}

/// A path is awaitable and not a coroutine: `create_task` takes its `_resolve()` with a name and
/// refuses the path itself, as asyncio does, while `gather` and `wait_for` take it as it is. The
/// facade's type owns no public name, so `close()` and `send()` are the host's.
#[test]
fn a_path_is_awaitable_and_not_a_coroutine() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            task = asyncio.create_task(stub.slow()._resolve(), name='slow')
            print(await task, task.get_name())
            try:
                asyncio.create_task(stub.slow())
            except TypeError as e:
                print('create_task:', e)
            print(await asyncio.gather(stub.a.b.c, stub.b))
            print(await asyncio.wait_for(stub.public, 30))
            print([n for n in ('send', 'throw', 'close') if hasattr(type(stub), n)])
            print(await stub.close(), await stub.send(1))
            try:
                await runtime.wait({stub.public})
            except TypeError as e:
                print('wait:', e)
            "#,
        ),
    );
    assert_eq!(
        out,
        "slow slow\n\
         create_task: a coroutine was expected, got <hosted fx.slow(...): unresolved, await it>\n\
         ['c', 'b']\n\
         public value\n\
         []\n\
         closed 1\n\
         wait: <hosted fx.public: unresolved, await it> is awaitable, not a future: wait on a \
         task, as `asyncio.create_task(x._resolve(), name=...)`\n"
    );
}

/// `hasattr` cannot see the host and `getattr` with a default never takes it, since a lookup
/// builds a path; the error arrives at the `await`, an `AttributeError` for a missing name and
/// for the binding's refusal of a private one alike.
#[test]
fn a_missing_or_refused_attribute_raises_attribute_error_where_awaited() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            print(hasattr(stub, 'missing'), repr(getattr(stub, 'missing', None)))
            for label, path in (('missing', stub.missing), ('private', stub._private)):
                try:
                    await path
                except AttributeError as e:
                    print(label, type(e).__name__, str(e).splitlines()[0])
            "#,
        ),
    );
    assert_eq!(
        out,
        "True <hosted fx.missing: unresolved, await it>\n\
         missing AttributeError 'Root' object has no attribute 'missing'\n\
         private outrig.Refused '_private' is not a public name, and it is not on RPyC's safe \
         list\n"
    );
}

/// `async with` enters the host's object, and an exception raised inside reaches its
/// `__exit__` by value, type and text, as a `with` on RPyC's proxy sends it.
#[test]
fn async_with_exits_with_the_exception_by_value() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            async with stub.manager as m:
                print(await m.entered)
            try:
                async with stub.manager:
                    raise ValueError('inside', 5)
            except ValueError as e:
                exits = [x async for x in stub.manager.exits]
                print('raised', e.args, exits[-1])
            "#,
        ),
    );
    assert_eq!(
        out,
        "1\nraised ('inside', 5) ('ValueError', \"ValueError('inside', 5)\", True)\n"
    );
}

/// Whether an exception raised inside `async with` is suppressed is the truth of what the host's
/// `__exit__` gives back, read on the host: a list, which crosses by reference, too.
#[test]
fn async_with_suppresses_by_the_truth_of_what_exit_gives_back() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            for name in ('suppressing', 'propagating'):
                try:
                    async with getattr(stub, name):
                        raise ValueError(name)
                    print(name, 'suppressed')
                except ValueError as e:
                    print(name, 'raised', e)
            "#,
        ),
    );
    assert_eq!(
        out,
        "suppressing suppressed\npropagating raised propagating\n"
    );
}

/// What the host's `__exit__` gives back is truth-tested only when the body raised, as `with` does
/// it: a clean body ends cleanly even when testing that value raises.
#[test]
fn async_with_tests_what_exit_gives_back_only_for_an_exception() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            async with stub.untestable:
                pass
            print('clean')
            try:
                async with stub.untestable:
                    raise KeyError('body')
            except Exception as e:
                print('raised', type(e).__name__)
            "#,
        ),
    );
    assert_eq!(out, "clean\nraised ValueError\n");
}

/// A resolved method, generator or class claims no type Python's machinery treats specially,
/// so `inspect` and asyncio take it for an ordinary object; `help` of each answers here and
/// sends nothing.
#[test]
fn a_resolved_proxy_claims_no_machinery_type() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import inspect
            f = await stub.method
            g = await stub.iterable()
            c = await stub.nested_type
            for x in (f, g, c):
                special = (inspect.isroutine(x) or inspect.isgenerator(x) or inspect.isclass(x)
                           or asyncio.iscoroutine(x) or inspect.ismodule(x))
                print(repr(x), x.__class__ is type(x), special)
            print((await asyncio.gather(g))[0] is g)
            print([x async for x in g])
            "#,
        ),
    );
    assert_eq!(
        out,
        "<hosted builtins.method from fx> True False\n\
         <hosted builtins.generator from fx> True False\n\
         <hosted class outrig_fixture.Nested from fx> True False\n\
         True\n\
         [1, 2, 3]\n"
    );
    let before = sent(&relay, "primary");
    let help = relay.output(2, "help(f)\nhelp(g)\nhelp(c)\nc");
    assert_eq!(sent(&relay, "primary"), before, "{help}");
    assert_eq!(help.matches("Help on HostedObject").count(), 3, "{help}");
    assert!(
        help.contains("The methods the host listed for builtins.method:"),
        "{help}"
    );
    assert!(
        help.ends_with("<hosted class outrig_fixture.Nested from fx>\n"),
        "{help}"
    );
}

/// `isinstance` against a hosted class answers here for an instance of exactly that class and
/// for an object of this process; anything else is the host's to say, which `_isinstance` asks.
#[test]
fn isinstance_of_a_hosted_class_answers_here_or_points_at_the_host() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(&format!(
            "{}\n{}",
            py(AWAITED_COUNTING),
            py(r#"
                c = await stub.nested_type
                nested, manager = await stub.nested, await stub.manager
                counts.clear()
                print(isinstance(nested, c), isinstance(5, c), dict(counts))
                try:
                    isinstance(manager, c)
                except TypeError as e:
                    print(e)
                print(await c._isinstance(manager), await c._isinstance(stub.nested))
            "#)
        )),
    );
    assert_eq!(
        out,
        "True False {}\n\
         whether <hosted outrig_fixture.Manager from fx> is an instance of <hosted class \
         outrig_fixture.Nested from fx> is the host's to say: `await x._isinstance(obj)` asks it\n\
         False True\n"
    );
}

/// A hosted object belongs to the kernel whose pool made it: another kernel can neither await
/// it nor pass it to the binding, and the host's method is not called.
#[test]
fn another_kernels_hosted_object_is_refused() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    relay.output(1, &with_stub("__main__.shared = stub.nested"));
    let out = relay.output_in(
        "b",
        1,
        &with_stub_in(
            "b",
            r#"
            for label, path in (('await', __main__.shared), ('pass', stub.takes(__main__.shared))):
                try:
                    await path
                except TypeError as e:
                    print(label, '->', e)
            print(await stub.taken)
            "#,
        ),
    );
    assert_eq!(
        out,
        "await -> a hosted object of agent 'primary' cannot be used by agent 'b': each agent \
         reaches a binding through its own\n\
         pass -> a proxy of binding 'fx' on agent 'primary' cannot be passed to binding 'fx' on \
         agent 'b': a hosted object crosses only to its own binding, from the kernel that holds \
         it\n\
         0\n"
    );
}

/// A 10 s awaited call in a task leaves its kernel's loop turning: `inv` is answered, a timer in
/// another task fires, and `runtime.wait` returns on a message the user sends meanwhile. One
/// worker thread serves the pool, and no thread is made for the call.
#[test]
fn a_long_awaited_call_leaves_the_loop_running() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (started, heard, release) = (Flag::new(), Flag::new(), Flag::new());
    relay.submit_in(
        "primary",
        1,
        &with_stub(&format!(
            r#"
            import threading
            call = asyncio.create_task(
                stub.record(10, 'long', started={started}, until={release})._resolve(), name='long'
            )
            timer = asyncio.create_task(asyncio.sleep(0.1, result='fired'), name='timer')
            {awaited}
            print(await timer)
            try:
                await runtime.wait({{call}})
            except runtime.MessageAvailable:
                print('message', (await runtime.channels['user'].receive()).body)
            open({heard}, 'w').close()
            print([t.name for t in threading.enumerate() if t.name.startswith('hosted-')])
            print(await call)
            "#,
            started = started.py(),
            release = release.py(),
            heard = heard.py(),
            awaited = started.awaited().replace('\n', "\n            "),
        )),
    );
    started.wait();
    let inventory = relay.inv("primary", 7);
    assert!(
        inventory["globals"]
            .as_array()
            .expect("rows")
            .iter()
            .any(|row| row[0] == "stub" && row[1] == "HostedObject"),
        "{inventory}"
    );
    relay.msg("primary", 8, "user", "hello");
    heard.wait();
    release.raise();
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(
        result["output"], "fired\nmessage hello\n['hosted-primary-fx_0']\nlong\n",
        "{result}"
    );
}

/// Four awaited calls from one kernel take four connections, and a fifth waits in the executor's
/// queue with no thread and no connection, running when one of the four returns.
#[test]
fn a_fifth_awaited_call_waits_for_a_worker() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub_pool_in(
            "primary",
            r#"
            import tempfile, threading
            d = tempfile.mkdtemp()
            starts = [os.path.join(d, f's{i}') for i in range(5)]
            ends = [os.path.join(d, f'e{i}') for i in range(4)]
            calls = [
                asyncio.create_task(
                    stub.record(0, f'c{i}', started=starts[i], until=ends[i])._resolve()
                )
                for i in range(4)
            ]
            for path in starts[:4]:
                await appears(path)
            fifth = asyncio.create_task(stub.record(0, 'c4', started=starts[4])._resolve())
            await asyncio.sleep(0.5)
            workers = [t.name for t in threading.enumerate() if t.name.startswith('hosted-')]
            print(os.path.exists(starts[4]), len(workers), numbers())
            touch(ends[0])
            print(await calls[0], await fifth, numbers())
            for path in ends[1:]:
                touch(path)
            print(await asyncio.gather(*calls[1:]))
            "#,
        ),
    );
    assert_eq!(
        out,
        "False 4 [1, 2, 3, 4]\nc0 c4 [1, 2, 3, 4]\n['c1', 'c2', 'c3']\n"
    );
    assert_eq!(ids_used(&relay, "primary"), [1, 2, 3, 4]);
}

/// Interrupt -- or cancel -- `agent`'s execution while it awaits a 10 s hosted call: the call
/// raises where it is awaited within a second, with 0003-17's text; the next call returns on
/// another connection, the interrupted one is free again once its late reply has come and gone,
/// and the host finished the call.
fn an_awaited_call_is_interrupted_where_it_is_awaited(agent: &str, cancel: bool) {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    if agent != "primary" {
        relay.open(agent);
    }
    let (started, release) = (Flag::new(), Flag::new());
    relay.submit_in(
        agent,
        1,
        &with_stub_in(
            agent,
            &format!(
                "print(await stub.record(10, 'slow', started={}, until={}))",
                started.py(),
                release.py()
            ),
        ),
    );
    started.wait();
    let asked = Instant::now();
    if cancel {
        relay.cancel(agent, 1);
    } else {
        relay.interrupt(agent, 1, false);
    }
    let result = relay.result_from(agent, 1, TIMEOUT);
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "{:?}",
        asked.elapsed()
    );
    let (raised, verb) = if cancel {
        ("CancelledError", "cancelled")
    } else {
        ("KeyboardInterrupt", "interrupted")
    };
    assert_eq!(result["raised"], raised, "{result}");
    let error = result["error"].as_str().expect("an error");
    assert!(
        error.contains(&format!(
            "{verb} by outrig while waiting for binding 'fx' to answer: the call's outcome on \
             the host is unknown"
        )),
        "{error}"
    );
    assert!(!release.exists());
    let out = relay.output_in(
        agent,
        2,
        &with_stub_pool_in(agent, "print(await stub.public, pending(), numbers())"),
    );
    assert_eq!(out, "public value [1] [1, 2]\n");
    release.raise();
    let out = relay.output_in(
        agent,
        3,
        &with_stub_pool_in(
            agent,
            r#"
            stale = next(c for c in pool.connections if c._request_callbacks)
            deadline = time.monotonic() + 20
            while not stale._channel.poll(0):
                if time.monotonic() > deadline:
                    raise TimeoutError('the late reply never came')
                await asyncio.sleep(0.01)
            print(await stub.method(1), pending(), numbers())
            "#,
        ),
    );
    assert_eq!(out, "2 [] [1, 2]\n");
    assert_eq!(ids_used(&relay, agent), [1, 2]);
    let recorded = intervals(&mut relay, agent, 4);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].label, "slow");
}

#[test]
fn an_awaited_call_is_interrupted_on_the_primary() {
    an_awaited_call_is_interrupted_where_it_is_awaited("primary", false);
}

#[test]
fn an_awaited_call_is_interrupted_on_a_child() {
    an_awaited_call_is_interrupted_where_it_is_awaited("b", false);
}

#[test]
fn an_awaited_call_is_cancelled_where_it_is_awaited() {
    an_awaited_call_is_interrupted_where_it_is_awaited("b", true);
}

/// An interrupt while the execution's call waits in the executor's queue -- four calls of an
/// earlier execution hold every worker -- says the call was never sent, and the host never
/// counts it; the earlier execution's calls run on.
#[test]
fn an_interrupted_queued_call_was_never_sent() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let release = Flag::new();
    let starts: Vec<Flag> = (0..4).map(|_| Flag::new()).collect();
    let tasks: String = starts
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "blockers.append(asyncio.create_task(stub.record(0, 'b{i}', started={}, \
                 until={})._resolve()))\n",
                s.py(),
                release.py()
            )
        })
        .collect();
    relay.output(1, &with_stub(&format!("blockers = []\n{tasks}")));
    for s in &starts {
        s.wait();
    }
    let queued = Flag::new();
    relay.submit_in(
        "primary",
        2,
        &format!(
            "open({}, 'w').close()\nprint(await stub.noted())",
            queued.py()
        ),
    );
    queued.wait();
    std::thread::sleep(Duration::from_millis(200));
    relay.interrupt("primary", 2, false);
    let result = relay.result_from("primary", 2, TIMEOUT);
    assert_eq!(result["raised"], "KeyboardInterrupt", "{result}");
    let error = result["error"].as_str().expect("an error");
    assert!(
        error.contains(
            "interrupted by outrig while waiting for a free connection to binding 'fx': the call \
             was never sent"
        ),
        "{error}"
    );
    release.raise();
    assert_eq!(
        relay.output(
            3,
            "print(await asyncio.gather(*blockers), await stub.calls)"
        ),
        "['b0', 'b1', 'b2', 'b3'] 0\n"
    );
}

/// Cancelling the task that awaits a hosted call, from agent code, raises asyncio's own
/// `CancelledError` and cancels the wait alone: the host's call runs on, and the next call tidies
/// its late reply away.
#[test]
fn cancelling_the_awaiting_task_cancels_the_wait_alone() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub_pool_in(
            "primary",
            r#"
            import tempfile
            d = tempfile.mkdtemp()
            started, release = os.path.join(d, 's'), os.path.join(d, 'r')
            t = asyncio.create_task(
                stub.record(10, 'abandoned', started=started, until=release)._resolve()
            )
            await appears(started)
            t.cancel()
            try:
                await t
            except asyncio.CancelledError as e:
                print('cancelled', e.args)
            print(await stub.public, pending(), numbers())
            touch(release)
            stale = next(c for c in pool.connections if c._request_callbacks)
            deadline = time.monotonic() + 20
            while not stale._channel.poll(0):
                if time.monotonic() > deadline:
                    raise TimeoutError('the late reply never came')
                await asyncio.sleep(0.01)
            print(await stub.method(1), pending(), numbers())
            "#,
        ),
    );
    assert_eq!(out, "cancelled ()\npublic value [1] [1, 2]\n2 [] [1, 2]\n");
    let recorded = intervals(&mut relay, "primary", 2);
    assert_eq!(recorded[0].label, "abandoned");
}

/// Cancelling the task that awaits a call ends the calls its worker makes inside it, while the
/// host's run on: one a coroutine callback awaits, nested on the outer call's connection, and one
/// a plain callback makes with `_sync()` on another binding. The worker lets go of every
/// connection at once rather than when those calls return.
#[test]
fn cancelling_an_awaited_call_ends_the_calls_made_inside_it() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.bind("fx2", FACTORY);
    let out = relay.output(
        1,
        &with_stub_pool_in(
            "primary",
            r#"
            import tempfile
            d = tempfile.mkdtemp()
            other = K.stub('fx2')
            async def let_go(label, call):
                started, release = os.path.join(d, label), os.path.join(d, label + '.r')
                t = asyncio.create_task(call(started, release)._resolve())
                await appears(started)
                t.cancel()
                try:
                    await t
                except asyncio.CancelledError:
                    pass
                deadline = time.monotonic() + 5
                while any(c.owner is not None for p in K._hosted.values() for c in p.connections):
                    if time.monotonic() > deadline:
                        raise TimeoutError(f'{label}: the worker stayed in the call inside')
                    await asyncio.sleep(0.01)
                print(label, 'let go')
                touch(release)
            async def awaiting(started, release):
                return await stub.record(10, 'nested', started=started, until=release)
            def syncing(started, release):
                return other.record(10, 'other', started=started, until=release)._sync()
            await let_go('coroutine', lambda s, r: stub.call(awaiting, s, r))
            await let_go('plain', lambda s, r: stub.call(syncing, s, r))
            "#,
        ),
    );
    assert_eq!(out, "coroutine let go\nplain let go\n");
}

/// A plain-function callback runs on the worker serving its call, in the awaiting execution's
/// context -- its output is that execution's, and a context variable set there reads the same --
/// and reads a host object it is given with `_sync()`. `_sync()` is refused on the loop.
#[test]
fn a_plain_callback_runs_on_the_worker_in_the_awaiting_context() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import contextvars, threading
            cv = contextvars.ContextVar('cv')
            cv.set('the awaiting task')
            def callback(x):
                print('in the callback', x)
                return threading.current_thread().name, cv.get()
            print(await stub.call(callback, 3))
            print(await stub.call_with_nested(lambda nested: (repr(nested), nested.value._sync())))
            try:
                stub.public._sync()
            except TypeError as e:
                print('refused:', str(e).split('answered: ')[1])
            "#,
        ),
    );
    assert_eq!(
        out,
        "in the callback 3\n\
         ('hosted-primary-fx_0', 'the awaiting task')\n\
         ('<hosted outrig_fixture.Nested from fx>', 7)\n\
         refused: `await` it instead. `_sync()` is for a plain-function callback, which runs on \
         a worker and cannot await\n"
    );
}

/// Four callbacks in flight at once, each in a call of its own, read the host object they were
/// given with `_sync()`, each on its own call's connection: nothing waits for a fifth.
#[test]
fn sync_reads_complete_with_every_connection_occupied() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import threading
            barrier = threading.Barrier(4, timeout=30)
            def read(nested):
                barrier.wait()
                return nested.value._sync()
            print(await asyncio.gather(*(stub.call_with_nested(read) for _ in range(4))))
            "#,
        ),
    );
    assert_eq!(out, "[7, 7, 7, 7]\n");
    assert_eq!(ids_used(&relay, "primary"), [1, 2, 3, 4]);
}

/// A callback the host makes inside a call that a plain callback of an awaited call makes with
/// `_sync()` -- here on another binding, serialized -- is given hosted objects too: a plain one
/// reads them with `_sync()`, and a coroutine awaits them inside the call that passed them.
#[test]
fn a_callback_inside_a_sync_call_is_given_hosted_objects() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.bind_serialized("fx2", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            other = K.stub('fx2')
            def plain(nested):
                return repr(nested), nested.value._sync()
            async def coroutine(nested):
                return repr(nested), await nested.value
            def inside():
                return tuple(other.call_with_nested(fn)._sync() for fn in (plain, coroutine))
            print(await stub.call(inside))
            "#,
        ),
    );
    let read = "('<hosted outrig_fixture.Nested from fx2>', 7)";
    assert_eq!(out, format!("({read}, {read})\n"));
}

/// A coroutine callback runs on the kernel's loop, and a hosted call it awaits travels on the
/// outer call's connection.
#[test]
fn a_coroutine_callback_runs_on_the_loop_and_calls_on_its_callers_connection() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import threading
            async def on_loop(x):
                return threading.current_thread().name, await stub.method(x)
            print(await stub.call(on_loop, 1))
            print(await stub.call_twice(on_loop, 2))
            "#,
        ),
    );
    assert_eq!(
        out,
        "('MainThread', 2)\n(('MainThread', 3), ('MainThread', 3))\n"
    );
    assert_eq!(ids_used(&relay, "primary"), [1]);
}

/// Under `serialize = true` a coroutine callback's own call goes through the turn the outer call
/// holds, and returns while the outer call is still in progress on the host.
#[test]
fn a_coroutine_callbacks_call_runs_inside_a_serialized_outer_call() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            async def inner():
                return await stub.record(0, 'inner')
            print(await stub.call_recorded(inner, 'outer'))
            "#,
        ),
    );
    assert_eq!(out, "inner\n");
    assert_eq!(ids_used(&relay, "primary"), [1]);
    let recorded = intervals(&mut relay, "primary", 2);
    let (outer, inner) = (interval(&recorded, "outer"), interval(&recorded, "inner"));
    assert!(
        outer.start <= inner.start && inner.end <= outer.end,
        "{recorded:?}"
    );
}

/// A plain callback of a call that a coroutine callback awaits runs in the coroutine's context,
/// as any awaiting task's: on the thread serving the coroutine, and on a worker for a call queued
/// too late for that thread to serve it.
#[test]
fn a_call_awaited_in_a_coroutine_callback_runs_in_its_context() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub_pool_in(
            "primary",
            r#"
            import contextvars, tempfile
            d = tempfile.mkdtemp()
            cv = contextvars.ContextVar('cv')
            cv.set('outer')
            def plain():
                return cv.get()
            async def served():
                cv.set('served')
                return await stub.call(plain)
            print(await stub.call(served))
            tasks = []
            async def late():
                cv.set('late')
                started, release = os.path.join(d, 's'), os.path.join(d, 'r')
                # The serving thread is still in the first call when this returns -- the release
                # follows the return -- so the second, queued behind it, goes to a worker.
                tasks.append(asyncio.create_task(
                    stub.record(0, 'first', started=started, until=release)._resolve()
                ))
                tasks.append(asyncio.create_task(stub.call(plain)._resolve()))
                await appears(started)
                asyncio.current_task().add_done_callback(lambda _: touch(release))
                return 'returned'
            print(await stub.call(late), await tasks[0], await tasks[1])
            "#,
        ),
    );
    assert_eq!(out, "served\nreturned first late\n");
}

/// A coroutine callback served inside a call that another coroutine callback awaits does not
/// hold up that callback's other tasks: a hosted call one of them awaits runs on the serving
/// thread meanwhile, on the same connection. It ends with the outer callback's call, not the
/// inner one's, so cancelling the inner one's call leaves it to finish.
#[test]
fn a_nested_coroutine_callback_does_not_hold_up_its_callers_other_tasks() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub_pool_in(
            "primary",
            r#"
            import tempfile
            d = tempfile.mkdtemp()
            async def setting():
                ready = asyncio.Event()
                async def waiter():
                    await asyncio.wait_for(ready.wait(), 10)
                    return 'waited'
                async def setter():
                    value = await stub.method(1)
                    ready.set()
                    return value
                return tuple(await asyncio.gather(stub.call(waiter), setter()))
            print(await stub.call(setting))
            async def cancelling():
                started, release = os.path.join(d, 's'), os.path.join(d, 'r')
                async def waiter():
                    await asyncio.sleep(30)
                inner = asyncio.create_task(stub.call(waiter)._resolve())
                sibling = asyncio.create_task(
                    stub.record(0, 'sibling', started=started, until=release)._resolve()
                )
                await appears(started)
                inner.cancel()
                await asyncio.sleep(0.2)
                touch(release)
                results = await asyncio.gather(inner, sibling, return_exceptions=True)
                return tuple(
                    type(r).__name__ if isinstance(r, BaseException) else r for r in results
                )
            print(await stub.call(cancelling))
            "#,
        ),
    );
    assert_eq!(out, "('waited', 2)\n('CancelledError', 'sibling')\n");
    assert_eq!(ids_used(&relay, "primary"), [1]);
}

/// The same with the inner coroutine callback served on another binding, inside a `_sync()` call
/// of a plain callback: a call the outer callback's other task awaits once the inner callback is
/// waiting still runs on the serving thread, which is waiting on the other binding's connection.
#[test]
fn a_coroutine_callback_on_another_binding_does_not_hold_up_its_callers_other_tasks() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.bind("fx2", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            other = K.stub('fx2')
            async def setting():
                entered, ready = asyncio.Event(), asyncio.Event()
                async def waiter():
                    entered.set()
                    await asyncio.wait_for(ready.wait(), 10)
                    return 'waited'
                def plain():
                    return other.call(waiter)._sync()
                async def setter():
                    await entered.wait()
                    value = await stub.method(1)
                    ready.set()
                    return value
                return tuple(await asyncio.gather(stub.call(plain), setter()))
            print(await stub.call(setting))
            "#,
        ),
    );
    assert_eq!(out, "('waited', 2)\n");
}

/// A plain callback of a call that a coroutine callback awaits may start a coroutine on the loop
/// and wait for it: what that coroutine awaits goes to a worker, not to the thread the callback
/// holds.
#[test]
fn a_plain_callback_may_wait_for_a_coroutine_it_starts() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            loop = asyncio.get_running_loop()
            async def started():
                return await stub.method(1)
            def plain():
                return asyncio.run_coroutine_threadsafe(started(), loop).result(timeout=10)
            async def served():
                return await stub.call(plain)
            print(await stub.call(served))
            "#,
        ),
    );
    assert_eq!(out, "2\n");
}

/// With both bindings serialized, a coroutine callback's call to another binding, whose own
/// coroutine callback calls back into the first, completes, as its synchronous spelling does: the
/// thread serving the outer callback runs the call to the other binding, so the call back goes on
/// the outer call's connection, through the turn it holds. The same from a plain callback's
/// `_sync()` call.
#[test]
fn a_call_back_through_another_serialized_binding_completes() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.bind_serialized("fx2", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            other = K.stub('fx2')
            async def leaf():
                return await stub.method(1)
            async def outer():
                return await other.call(leaf)
            print(await asyncio.wait_for(stub.call(outer), 10))
            def plain():
                return other.call(leaf)._sync()
            print(await asyncio.wait_for(stub.call(plain), 10))
            "#,
        ),
    );
    assert_eq!(out, "2\n2\n");
    assert_eq!(ids_used(&relay, "primary"), [1]);
}

/// A call to another binding that a coroutine callback awaits runs on the thread serving it, and
/// one queued too late for that thread goes to a worker of its own binding.
#[test]
fn a_call_to_another_binding_queued_too_late_goes_to_its_own_workers() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.bind("fx2", FACTORY);
    let out = relay.output(
        1,
        &with_stub_pool_in(
            "primary",
            r#"
            import tempfile
            d = tempfile.mkdtemp()
            other = K.stub('fx2')
            def name():
                return threading.current_thread().name
            tasks = []
            async def late():
                started, release = os.path.join(d, 's'), os.path.join(d, 'r')
                served = await other.call(name)
                # The serving thread is still in the first call when this returns -- the release
                # follows the return -- so the second, queued behind it, goes to a worker.
                tasks.append(asyncio.create_task(
                    stub.record(0, 'first', started=started, until=release)._resolve()
                ))
                tasks.append(asyncio.create_task(other.call(name)._resolve()))
                await appears(started)
                asyncio.current_task().add_done_callback(lambda _: touch(release))
                return served
            print(await stub.call(late), await tasks[0], await tasks[1])
            "#,
        ),
    );
    assert_eq!(out, "hosted-primary-fx_0 first hosted-primary-fx2_0\n");
}

/// An interrupt while a coroutine callback awaits a hosted call of its own ends the execution
/// with the interrupt.
#[test]
fn an_interrupt_inside_a_coroutine_callback_ends_the_execution() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (started, release) = (Flag::new(), Flag::new());
    relay.submit_in(
        "primary",
        1,
        &with_stub(&format!(
            r#"
            async def stuck():
                await stub.record(10, 'inner', started={}, until={})
            await stub.call(stuck)
            "#,
            started.py(),
            release.py()
        )),
    );
    started.wait();
    relay.interrupt("primary", 1, false);
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["raised"], "KeyboardInterrupt", "{result}");
    release.raise();
    assert_eq!(relay.output(2, "print(await stub.method(1))"), "2\n");
}

/// An interrupt of an execution awaiting two calls through `gather` ends both. Each task `gather`
/// made lets its interrupt out of the loop, as asyncio does with one, and each is reported to the
/// execution as a task awaiting a hosted call -- none escapes unclaimed.
#[test]
fn an_interrupt_ends_every_call_a_gather_awaits() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (a, b, release) = (Flag::new(), Flag::new(), Flag::new());
    relay.submit_in(
        "primary",
        1,
        &with_stub(&format!(
            "print(await asyncio.gather(stub.record(10, 'a', started={a}, until={r}), \
             stub.record(10, 'b', started={b}, until={r})))",
            a = a.py(),
            b = b.py(),
            r = release.py()
        )),
    );
    a.wait();
    b.wait();
    relay.interrupt("primary", 1, false);
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["raised"], "KeyboardInterrupt", "{result}");
    release.raise();
    let next = relay.exec(2, "pass");
    let reported = format!("{}{}", result["output"], next["background"]);
    assert_eq!(
        reported
            .matches("[outrig interrupted a task awaiting a hosted call]")
            .count(),
        2,
        "{result}\n{next}"
    );
    assert!(
        !relay.stderr().contains("escaped its event loop"),
        "{}",
        relay.stderr()
    );
}

/// Closing the pool while a coroutine callback runs on the loop -- asleep, awaiting no hosted
/// call -- ends the awaited call with the closing error at once, and the thread serving the
/// callback cancels it and lets go.
#[test]
fn closing_the_pool_ends_a_coroutine_callback_it_was_serving() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("c");
    let ready = Flag::new();
    relay.submit_in(
        "c",
        1,
        &with_stub_pool_in(
            "c",
            &format!(
                r#"
                async def napping():
                    await asyncio.sleep(30)
                call = asyncio.create_task(stub.call(napping)._resolve())
                def serving():
                    return [t for t in asyncio.all_tasks() if t.get_coro().__name__ == 'napping']
                deadline = time.monotonic() + 20
                while not serving():
                    if time.monotonic() > deadline:
                        raise TimeoutError('the callback never started')
                    await asyncio.sleep(0.01)
                touch({ready})
                try:
                    await call
                except EOFError as e:
                    print(e)
                while serving() and not all(t.done() for t in serving()):
                    if time.monotonic() > deadline:
                        raise TimeoutError('the callback was never cancelled')
                    await asyncio.sleep(0.01)
                print('the callback ended')
                "#,
                ready = ready.py()
            ),
        ),
    );
    ready.wait();
    relay.output(2, "import __main__\n__main__._kernels['c'].close_hosted()");
    let result = relay.result_from("c", 1, TIMEOUT);
    assert_eq!(
        result["output"],
        "the kernel's connections are closing: the call's outcome on the host is unknown\n\
         the callback ended\n",
        "{result}"
    );
}

/// The release of a kernel -- `close_hosted`, from another thread -- while two awaited calls are
/// in flight and the engine holds the pool's other two connections: each awaited call raises
/// `EOFError` saying its outcome is unknown, a third waiting for a connection was never sent,
/// and the binding finishes the calls.
#[test]
fn closing_the_pool_settles_awaited_calls_in_flight_and_waiting() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("c");
    let ready = Flag::new();
    relay.submit_in(
        "c",
        1,
        &with_stub_pool_in(
            "c",
            &format!(
                r#"
                import tempfile
                d = tempfile.mkdtemp()
                release = os.path.join(d, 'release')
                starts = [os.path.join(d, f's{{i}}') for i in range(4)]
                root = K.hosted('fx')
                fill = [
                    worker(lambda i=i: root.record(0, f'fill{{i}}', started=starts[i],
                                                   until=release))
                    for i in range(2)
                ]
                calls = [
                    asyncio.create_task(
                        stub.record(0, f'call{{i}}', started=starts[2 + i], until=release)._resolve()
                    )
                    for i in range(2)
                ]
                for path in starts:
                    await appears(path)
                third = asyncio.create_task(stub.noted()._resolve())
                deadline = time.monotonic() + 20
                while not any(job.waiting and job.conn is None for job in list(pool.jobs)):
                    if time.monotonic() > deadline:
                        raise TimeoutError('the third call never waited for a connection')
                    await asyncio.sleep(0.01)
                touch({ready})
                results = await asyncio.gather(*calls, third, *fill, return_exceptions=True)
                for r in results:
                    print(f'{{type(r).__name__}}: {{r}}')
                touch(release)
                "#,
                ready = ready.py()
            ),
        ),
    );
    ready.wait();
    relay.output(2, "import __main__\n__main__._kernels['c'].close_hosted()");
    let result = relay.result_from("c", 1, TIMEOUT);
    let unknown =
        "EOFError: the kernel's connections are closing: the call's outcome on the host is unknown";
    let never = "EOFError: the kernel's connections are closing: the call was never sent";
    assert_eq!(
        result["output"],
        format!("{unknown}\n{unknown}\n{never}\n{unknown}\n{unknown}\n"),
        "{result}"
    );
    eventually(
        || {
            let recorded = intervals(&mut relay, "primary", 3);
            (["call0", "call1", "fill0", "fill1"]
                .iter()
                .all(|label| recorded.iter().any(|i| &i.label == label)))
            .then_some(())
        },
        || "the four calls to finish on the host".to_string(),
    );
    assert_eq!(relay.output(4, &with_root("print(root.calls)")), "0\n");
}

/// The same close with every worker busy: four awaited calls in flight raise that their outcome
/// is unknown, and a fifth still in the executor's queue that it was never sent.
#[test]
fn closing_the_pool_settles_a_call_still_queued_for_a_worker() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("c");
    let ready = Flag::new();
    relay.submit_in(
        "c",
        1,
        &with_stub_pool_in(
            "c",
            &format!(
                r#"
                import tempfile
                d = tempfile.mkdtemp()
                release = os.path.join(d, 'release')
                starts = [os.path.join(d, f's{{i}}') for i in range(4)]
                calls = [
                    asyncio.create_task(
                        stub.record(0, f'call{{i}}', started=starts[i], until=release)._resolve()
                    )
                    for i in range(4)
                ]
                for path in starts:
                    await appears(path)
                fifth = asyncio.create_task(stub.noted()._resolve())
                await asyncio.sleep(0.2)
                touch({ready})
                results = await asyncio.gather(*calls, fifth, return_exceptions=True)
                for r in results:
                    print(f'{{type(r).__name__}}: {{r}}')
                touch(release)
                "#,
                ready = ready.py()
            ),
        ),
    );
    ready.wait();
    relay.output(2, "import __main__\n__main__._kernels['c'].close_hosted()");
    let result = relay.result_from("c", 1, TIMEOUT);
    let unknown =
        "EOFError: the kernel's connections are closing: the call's outcome on the host is unknown";
    let never = "EOFError: the kernel's connections are closing: the call was never sent";
    assert_eq!(
        result["output"],
        format!("{unknown}\n{unknown}\n{unknown}\n{unknown}\n{never}\n"),
        "{result}"
    );
    assert_eq!(relay.output(3, &with_root("print(root.calls)")), "0\n");
}

/// A kernel whose pool has idle workers -- which are not daemon threads -- still exits when its
/// stdin closes.
#[test]
fn an_interpreter_with_idle_workers_exits_when_its_stdin_closes() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import threading
            print(await asyncio.gather(stub.public, stub.b, stub.a.b.c))
            print(sum(t.name.startswith('hosted-') and not t.daemon for t in threading.enumerate()))
            "#,
        ),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "['public value', 'b', 'c']");
    assert!(lines[1].parse::<usize>().expect("a count") >= 1, "{out}");
    relay.close_stdin();
    let status = relay
        .exit_within(Duration::from_secs(10))
        .expect("the interpreter exits");
    assert!(status.success(), "{status:?}");
}

/// Nothing the kernel boots resolves a hosted object synchronously: no name bound at boot, nor a
/// public attribute of one, is a kernel, a pool, a connection, RPyC's proxy, or a method of one.
#[test]
fn no_name_a_kernel_boots_resolves_a_hosted_object_synchronously() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    let probe = r#"
        import __main__, inspect
        K = __main__._kernels[AGENT]
        engine = (__main__.Kernel, __main__._Pool, __main__._Hosted, __main__._Job,
                  __main__._rpyc.core.netref.BaseNetref)
        def engine_of(value):
            owner = getattr(value, '__self__', None) if inspect.ismethod(value) else None
            return any(isinstance(v, engine) or v in engine for v in (value, owner))
        found = []
        for name, value in K._boot.items():
            if engine_of(value):
                found.append(name)
            if name == '__builtins__' or inspect.ismodule(value):
                continue
            for attr in dir(value):
                if attr.startswith('_'):
                    continue
                try:
                    member = getattr(value, attr)
                except Exception:
                    continue
                if engine_of(member):
                    found.append(f'{name}.{attr}')
        print(found)
    "#;
    for agent in ["primary", "b"] {
        assert_eq!(
            relay.output_in(agent, 1, &format!("AGENT = {agent:?}\n{}", py(probe))),
            "[]\n",
            "{agent}"
        );
    }
}
