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
/// arguments already boxed, its frame labeled as the interpreter labels one; `idp` for a proxy's
/// id_pack; the labels; and `attempt`, which prints what a call gave or how it was refused.
const RAW: &str = r#"
    import rpyc, struct, zlib
    from rpyc.core import consts
    conn = object.__getattribute__(root, "____conn__")
    idp = lambda proxy: object.__getattribute__(proxy, "____id_pack__")
    L, V, T, R = (consts.LABEL_LOCAL_REF, consts.LABEL_VALUE, consts.LABEL_TUPLE,
                  consts.LABEL_REMOTE_REF)
    ops = {v: k[len("HANDLE_"):].lower()
           for k, v in vars(consts).items() if k.startswith("HANDLE_")}
    def raw_on(on, handler, boxed):
        res = rpyc.core.async_.AsyncResult(on)
        seq = on._get_seq_id()
        on._request_callbacks[seq] = res
        op = ops.get(handler, "unknown") if type(handler) is int else "unknown"
        on._send(consts.MSG_REQUEST, seq, (handler, boxed), {"request": seq, "op": op})
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

/// An object that makes up every attribute it lacks -- an attribute-dict library's, as
/// `addict.Dict` -- is handed out under an id the host makes, never one it asks the object for: as
/// the root, and as a child a read returns.
#[test]
fn an_object_that_makes_up_attributes_is_handed_out_as_any_other() {
    let mut relay = Relay::start();
    relay.bind("fx", "outrig_fixture:make_vivid");
    let out = relay.output(
        1,
        &with_root(
            r#"
            print(root["name"])
            child = root.child
            child["n"] = 1
            print(root.child["n"], root["child"]["n"])
            print(sorted(root.keys()))
            "#,
        ),
    );
    assert_eq!(out, "root\n1 1\n['child', 'name']\n");
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

/// A frame with no data, which RPyC never sends and would take without dispatching, closes its
/// connection the same way.
#[test]
fn a_frame_with_no_data_closes_the_connection_which_reopens() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            conn._channel._write(struct.pack("!LB", 0, 0) + b"\n", False)
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
        out.starts_with("closed: the host closed this connection: a frame with no data\n"),
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
    relay.inject_to_binding(json!({
        "t": "rpc", "binding": "fx", "agent": "primary", "id": 2,
        "data": base64(&vec![b'x'; PART_MAX + 1]), "more": true,
    }));
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
            def failing(part, more, label=None):
                if more:
                    raise OSError("the pipe tore")
                real_write(part, more, label)
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
        ("serve-fx-primary-1", "serve-fx-b-1")
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
    assert_eq!(threads, ["serve-fx-primary-1", "serve-fx-primary-2"]);
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
        outer, inner = turn.taking(), turn.taking()
        outer.__enter__()
        inner.__enter__()  # re-entrant for the holder: nobody is queued
        queued_by_holder = len(turn._waiters)
        inner.__exit__(None, None, None)
        def waiter(i):
            with turn.taking():
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
        outer.__exit__(None, None, None)  # depth 1 -> 0: the head of the queue takes it
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

/// An interrupt that finds `agent`'s thread decoding a reply -- the frame taken from the channel,
/// its request not yet answered -- is not raised there, which would leave the connection waiting
/// for a reply already read. The call raises it as it ends, and the connection serves the next
/// call. On the primary the signal leaves it for the call, and on a child the reader thread does.
/// The decoding is held in RPyC's own `load`, wrapped in code compiled under a name of its own,
/// until the interrupt is left for the call.
fn an_interrupt_while_a_reply_is_decoded_is_raised_as_the_call_ends(agent: &str) {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    if agent != "primary" {
        relay.open(agent);
    }
    let decoding = Flag::new();
    relay.submit_in(
        agent,
        1,
        &with_pool_in(
            agent,
            &format!(
                r#"
                import __main__, rpyc
                brine = rpyc.core.brine
                scope = dict(brine=brine, load=brine.load, threading=threading, time=time,
                             touch=touch, waits=__main__._waits, DECODING={decoding})
                exec(compile(
                    'def slow(data):\n'
                    '    brine.load = load\n'
                    '    touch(DECODING)\n'
                    '    deadline = time.monotonic() + 20\n'
                    '    while waits[threading.get_ident()].exc is None:\n'
                    '        if time.monotonic() > deadline:\n'
                    '            raise TimeoutError("the interrupt never came")\n'
                    '        time.sleep(0.01)\n'
                    '    return load(data)\n',
                    '<decoder>', 'exec'), scope)
                brine.load = scope['slow']
                try:
                    root.method(1)
                except KeyboardInterrupt as e:
                    print('KeyboardInterrupt:', e)
                print(pending(), numbers())
                print(root.method(1), pending(), numbers())
                "#,
                decoding = decoding.py()
            ),
        ),
    );
    decoding.wait();
    relay.interrupt(agent, 1, false);
    let result = relay.result_from(agent, 1, TIMEOUT);
    assert_eq!(
        result["output"],
        "KeyboardInterrupt: interrupted by outrig after the call to binding 'fx' ended\n[] [1]\n\
         2 [] [1]\n",
        "{result}"
    );
}

#[test]
fn an_interrupt_while_a_reply_is_decoded_on_the_primary_is_raised_as_the_call_ends() {
    an_interrupt_while_a_reply_is_decoded_is_raised_as_the_call_ends("primary");
}

#[test]
fn an_interrupt_while_a_reply_is_decoded_on_a_child_is_raised_as_the_call_ends() {
    an_interrupt_while_a_reply_is_decoded_is_raised_as_the_call_ends("b");
}

/// An interrupt that finds the primary's thread in a callback's code raises there at once -- in
/// code an imported module defines too -- and only there: a callback that catches it and returns
/// lets its call finish, with no second `KeyboardInterrupt` when the call next waits.
#[test]
fn an_interrupt_raises_in_a_callbacks_code_once() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let spinning = Flag::new();
    relay.submit_in(
        "primary",
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                scope = dict(touch=touch, SPINNING={spinning})
                exec(compile(
                    'def spin():\n'
                    '    touch(SPINNING)\n'
                    '    try:\n'
                    '        while True:\n'
                    '            pass\n'
                    '    except KeyboardInterrupt as e:\n'
                    '        return f"caught: {{e}}"\n',
                    '<module>', 'exec'), scope)
                print(root.call(scope['spin']))
                print(pending(), numbers())
                "#,
                spinning = spinning.py()
            ),
        ),
    );
    spinning.wait();
    relay.interrupt("primary", 1, false);
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(
        result["output"], "caught: interrupted by outrig\n[] [1]\n",
        "{result}"
    );
}

/// An interrupt that finds the primary's thread in an object's own code as a call boxes it -- a
/// path's `__fspath__` that never returns, of a class a module defines -- raises there. Boxing an
/// argument, it raises before anything of the request is written, and the connection serves the
/// next call; boxing a callback's result, it is the callback's answer as well as the call's error.
#[test]
fn an_interrupt_raises_in_an_objects_own_code_as_a_call_boxes_it() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (argument, result) = (Flag::new(), Flag::new());
    relay.submit_in(
        "primary",
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                scope = dict(touch=touch)
                exec(compile(
                    'class Looping:\n'
                    '    def __init__(self, flag):\n'
                    '        self.flag = flag\n'
                    '    def __fspath__(self):\n'
                    '        touch(self.flag)\n'
                    '        while True:\n'
                    '            pass\n',
                    '<module>', 'exec'), scope)
                Looping = scope['Looping']
                try:
                    root.method(Looping({argument}))
                except KeyboardInterrupt as e:
                    print('argument:', e)
                print(pending(), numbers(), root.method(1), pending(), numbers())
                try:
                    root.call(lambda: Looping({result}))
                except KeyboardInterrupt as e:
                    print('result:', e)
                "#,
                argument = argument.py(),
                result = result.py()
            ),
        ),
    );
    argument.wait();
    relay.interrupt("primary", 1, false);
    result.wait();
    relay.interrupt("primary", 1, false);
    let out = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(out["status"], "ok", "{out}");
    assert_eq!(
        out["output"],
        "argument: interrupted by outrig\n[] [1] 2 [] [1]\nresult: interrupted by outrig\n",
        "{out}"
    );
}

/// An interrupt that lands as a callback's own hosted call takes the pool's lock -- acquired, the
/// call not yet entered -- is not raised there, which would leave the lock held for good and the
/// reader thread stopped at the next frame for the pool. It is left on the call the callback
/// belongs to, and the callback's call, which has not begun, is not sent: it raises the interrupt
/// as that call, once. The signal is aimed and armed here, from the lock's `__enter__`, as the
/// reader thread aims and arms one.
#[test]
fn an_interrupt_as_a_callbacks_call_takes_the_pools_lock_leaves_the_lock_free() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let written = Flag::new();
    let out = relay.output(
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
            import __main__, signal
            scope = dict(threading=threading, signal=signal, kernel=__main__._primary)
            exec(compile(
                'class Slow(threading.Condition):\n'
                '    armed = False\n'
                '    def __enter__(self):\n'
                '        held = super().__enter__()\n'
                '        if Slow.armed and threading.current_thread() is threading.main_thread():\n'
                '            Slow.armed = False\n'
                '            kernel._interrupting = (1, False)\n'
                '            signal.pthread_kill(threading.get_ident(), signal.SIGINT)\n'
                '        return held\n',
                '<lock>', 'exec'), scope)
            Slow = scope['Slow']
            pool.cond = Slow(pool.cond._lock)
            for conn in pool.connections:
                conn._channel._cond = pool.cond
            def cb():
                Slow.armed = True
                return root.touch({written})
            try:
                root.call(cb)
            except KeyboardInterrupt as e:
                print('KeyboardInterrupt:', e)
            def probe():
                got = pool.cond.acquire(timeout=1)
                if got:
                    pool.cond.release()
                return got
            print(await worker(probe), root.method(1))
            "#,
                written = written.py()
            ),
        ),
    );
    assert_eq!(
        out,
        "KeyboardInterrupt: interrupted by outrig while waiting for binding 'fx' to answer: the \
         call's outcome on the host is unknown\nTrue 2\n"
    );
    assert!(
        !written.exists(),
        "the callback's write ran after the interrupt"
    );
}

/// An interrupt that finds the primary's thread in the code that rebuilds a host exception -- a
/// module's import, a class's `__new__` or `__init_subclass__`, an attribute's setter, which RPyC
/// runs under `vinegar.load` -- raises there, so a hook that never returns does not hold the
/// thread. The reply was taken and its request answered before the hook ran, so the connection
/// serves the next call.
#[test]
fn an_interrupt_raises_in_the_code_that_rebuilds_a_hosted_exception() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let hooked = Flag::new();
    relay.submit_in(
        "primary",
        1,
        &with_stub_pool_in(
            "primary",
            &format!(
                r#"
                import sys, types
                scope = dict(touch=touch, HOOKED={hooked})
                exec(compile(
                    'class FixtureError(Exception):\n'
                    '    def __init_subclass__(cls, **kwargs):\n'
                    '        touch(HOOKED)\n'
                    '        while True:\n'
                    '            pass\n',
                    '<module>', 'exec'), scope)
                module = types.ModuleType('outrig_fixture')
                module.FixtureError = scope['FixtureError']
                sys.modules['outrig_fixture'] = module
                try:
                    stub.raise_fixture()
                except KeyboardInterrupt as e:
                    print('KeyboardInterrupt:', e)
                print(pending(), numbers(), stub.method(1), pending(), numbers())
                "#,
                hooked = hooked.py()
            ),
        ),
    );
    hooked.wait();
    relay.interrupt("primary", 1, false);
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(
        result["output"], "KeyboardInterrupt: interrupted by outrig\n[] [1] 2 [] [1]\n",
        "{result}"
    );
}

/// A cancel that finds the primary's thread running a callback's code is left on the call the
/// callback belongs to, which raises it once the callback has returned. A hosted call the callback
/// makes before then -- a write, here -- is not sent: it raises the cancel, as that call and once,
/// and the host never hears of it.
#[test]
fn a_callbacks_call_after_a_cancel_landed_in_its_code_is_never_sent() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (waiting, written) = (Flag::new(), Flag::new());
    relay.submit_in(
        "primary",
        1,
        &with_stub_pool_in(
            "primary",
            &format!(
                r#"
                import __main__
                scope = dict(touch=touch, stub=stub, threading=threading, time=time,
                             waits=__main__._waits, WAITING={waiting}, WRITTEN={written})
                exec(compile(
                    'def cb():\n'
                    '    touch(WAITING)\n'
                    '    deadline = time.monotonic() + 20\n'
                    '    while waits[threading.get_ident()].exc is None:\n'
                    '        if time.monotonic() > deadline:\n'
                    '            raise TimeoutError("the cancel never came")\n'
                    '        time.sleep(0.01)\n'
                    '    return stub.touch(WRITTEN)\n',
                    '<module>', 'exec'), scope)
                try:
                    stub.call(scope['cb'])
                except asyncio.CancelledError as e:
                    print('CancelledError:', e)
                "#,
                waiting = waiting.py(),
                written = written.py()
            ),
        ),
    );
    waiting.wait();
    relay.cancel("primary", 1);
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(
        result["output"],
        "CancelledError: cancelled by outrig while waiting for binding 'fx' to answer: the \
         call's outcome on the host is unknown\n",
        "{result}"
    );
    assert!(
        !written.exists(),
        "the callback's write ran after the cancel"
    );
}

/// A callback raises an exception whose public property makes a hosted call, which RPyC reads to
/// box the exception for the host, after a cancel landed in the callback's code. That call is not
/// sent, as no call of a callback's is once a cancel waits for it; the host still gets an answer
/// to its call of the callback, an error that says what could not be sent, and the cancel is
/// raised once, as the call's. Without the answer the host's call stays in the callback holding
/// the binding's turn, which a call on another connection waits for.
#[test]
fn a_cancel_landing_as_a_callbacks_error_is_boxed_still_answers_the_host() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.open("b");
    relay.output(1, &with_root("root.public"));
    relay.output_in("b", 1, &with_root_in("b", "root.public"));
    let waiting = Flag::new();
    relay.submit_in(
        "primary",
        2,
        &with_stub_pool_in(
            "primary",
            &format!(
                r#"
                import __main__
                scope = dict(stub=stub, touch=touch, threading=threading, time=time,
                             waits=__main__._waits, WAITING={waiting})
                exec(compile(
                    'class Diagnosed(Exception):\n'
                    '    @property\n'
                    '    def detail(self):\n'
                    '        return stub.public\n'
                    'def cb():\n'
                    '    touch(WAITING)\n'
                    '    deadline = time.monotonic() + 20\n'
                    '    while waits[threading.get_ident()].exc is None:\n'
                    '        if time.monotonic() > deadline:\n'
                    '            raise TimeoutError("the cancel never came")\n'
                    '        time.sleep(0.01)\n'
                    '    raise Diagnosed("failed")\n',
                    '<module>', 'exec'), scope)
                try:
                    stub.call(scope['cb'])
                except asyncio.CancelledError as e:
                    print('CancelledError:', e)
                "#,
                waiting = waiting.py()
            ),
        ),
    );
    waiting.wait();
    relay.cancel("primary", 2);
    let result = relay.result_from("primary", 2, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(
        result["output"],
        "CancelledError: cancelled by outrig while waiting for binding 'fx' to answer: the \
         call's outcome on the host is unknown\n",
        "{result}"
    );
    assert_eq!(
        relay.output_in("b", 2, &with_root_in("b", "print(root.record(0, 'b'))")),
        "b\n"
    );
}

/// An interrupt that finds the primary's thread in a property RPyC reads to box a callback's
/// error -- a getter that loops, say -- raises there, as one that finds the code a callback runs
/// does. The host's call of the callback is answered with an error that says what could not be
/// sent, and the interrupt is the result of the call it belongs to.
#[test]
fn an_interrupt_raises_in_a_property_read_to_box_a_callbacks_error() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    let reading = Flag::new();
    relay.submit_in(
        "primary",
        1,
        &with_stub_pool_in(
            "primary",
            &format!(
                r#"
                scope = dict(touch=touch, READING={reading})
                exec(compile(
                    'class Diagnosed(Exception):\n'
                    '    @property\n'
                    '    def detail(self):\n'
                    '        touch(READING)\n'
                    '        while True:\n'
                    '            pass\n'
                    'def cb():\n'
                    '    raise Diagnosed("failed")\n',
                    '<module>', 'exec'), scope)
                try:
                    stub.call(scope['cb'])
                except KeyboardInterrupt as e:
                    print('KeyboardInterrupt:', e)
                print(stub.method(1))
                "#,
                reading = reading.py()
            ),
        ),
    );
    reading.wait();
    relay.interrupt("primary", 1, false);
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(
        result["output"], "KeyboardInterrupt: interrupted by outrig\n2\n",
        "{result}"
    );
}

/// The same property, read to send the exception a `with` block leaves a hosted context manager
/// by, is interrupted as well: before the `__exit__` request is written, so the exception the
/// interrupt raises is the block's and the connection has nothing outstanding.
#[test]
fn an_interrupt_raises_in_a_property_read_to_send_an_exception_to_exit() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let reading = Flag::new();
    relay.submit_in(
        "primary",
        1,
        &with_stub_pool_in(
            "primary",
            &format!(
                r#"
                scope = dict(touch=touch, READING={reading})
                exec(compile(
                    'class Diagnosed(Exception):\n'
                    '    @property\n'
                    '    def detail(self):\n'
                    '        touch(READING)\n'
                    '        while True:\n'
                    '            pass\n',
                    '<module>', 'exec'), scope)
                try:
                    with stub.manager:
                        raise scope['Diagnosed']('failed')
                except KeyboardInterrupt as e:
                    print('KeyboardInterrupt:', e)
                print(pending(), numbers(), stub.method(1), len(stub.manager.exits))
                "#,
                reading = reading.py()
            ),
        ),
    );
    reading.wait();
    relay.interrupt("primary", 1, false);
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(
        result["output"], "KeyboardInterrupt: interrupted by outrig\n[] [1] 2 0\n",
        "{result}"
    );
}

/// The host calls a callback after the caller of its call gave up -- the cancel notice, held back
/// here, was still on its way -- and the request is refused when the next call tidies the
/// connection. A SIGINT that lands as the refusal is built, aimed at the first call of RPyC's
/// `vinegar.dump` the way a real one could land in it, cannot lose the answer: the host's call of
/// the callback ends and the binding's turn passes to the other kernel's call.
#[test]
fn an_interrupt_cannot_lose_the_refusal_of_a_callback_the_host_calls_late() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.open("b");
    relay.output(1, &with_root("root.public"));
    relay.output_in("b", 1, &with_root_in("b", "root.public"));
    let started = Flag::new();
    relay.submit_in(
        "primary",
        2,
        &with_stub_pool_in(
            "primary",
            &format!(
                r#"
                import __main__, signal, sys
                from rpyc.core import vinegar
                main = threading.get_ident()
                __main__._Hosted.cancel = lambda self, seqs: None
                ran = []
                try:
                    stub.call_after(1, lambda: ran.append(1), {started})
                except asyncio.CancelledError:
                    print('CancelledError')
                deadline = time.monotonic() + 20
                while not any(c._channel.ready for c in pool.connections):
                    if time.monotonic() > deadline:
                        raise TimeoutError('the late callback never came')
                    time.sleep(0.01)
                def aim(frame, event, arg):
                    if event == 'call' and frame.f_code is vinegar.dump.__code__:
                        __main__._primary._interrupting = (2, False)
                        signal.pthread_kill(main, signal.SIGINT)
                sys.setprofile(aim)
                try:
                    print(ran, stub.method(1))
                except KeyboardInterrupt:
                    print('KeyboardInterrupt')
                finally:
                    sys.setprofile(None)
                "#,
                started = started.py()
            ),
        ),
    );
    started.wait();
    relay.cancel("primary", 2);
    let result = relay.result_from("primary", 2, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["output"], "CancelledError\n[] 2\n", "{result}");
    assert_eq!(
        relay.output_in("b", 2, &with_root_in("b", "print(root.record(0, 'b'))")),
        "b\n"
    );
}

/// A callback raises an interrupt whose class -- the `__class__` RPyC reads, and `isinstance`
/// with it -- loops. An interrupt lands in the getter, wherever the container asks for it, and the
/// host's call of the callback is answered. Told apart by type, the interrupt the callback
/// raised is not asked about before it is boxed, where nothing would end the getter.
#[test]
fn an_interrupt_raises_in_the_class_of_an_interrupt_a_callback_raised() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    let reading = Flag::new();
    relay.submit_in(
        "primary",
        1,
        &with_stub_pool_in(
            "primary",
            &format!(
                r#"
                scope = dict(touch=touch, READING={reading})
                exec(compile(
                    'class Odd(KeyboardInterrupt):\n'
                    '    @property\n'
                    '    def __class__(self):\n'
                    '        touch(READING)\n'
                    '        while True:\n'
                    '            pass\n'
                    'def cb():\n'
                    '    raise Odd()\n',
                    '<module>', 'exec'), scope)
                try:
                    stub.call(scope['cb'])
                except KeyboardInterrupt as e:
                    print('KeyboardInterrupt', type(e).__name__)
                print(stub.method(1))
                "#,
                reading = reading.py()
            ),
        ),
    );
    reading.wait();
    relay.interrupt("primary", 1, false);
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["output"], "KeyboardInterrupt Odd\n2\n", "{result}");
}

/// A callback raises a cancel whose `__class__` raises another: nothing asks the instance what it
/// is before it is boxed, so the host's call of the callback is answered, and the cancel the
/// callback raised is the call's result.
#[test]
fn a_class_hook_that_raises_on_a_control_error_a_callback_raised_is_answered() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.open("b");
    relay.output(1, &with_root("root.public"));
    relay.output_in("b", 1, &with_root_in("b", "root.public"));
    let out = relay.output(
        2,
        &with_stub_pool_in(
            "primary",
            r#"
            scope = dict(asyncio=asyncio)
            exec(compile(
                'class Odd(asyncio.CancelledError):\n'
                '    @property\n'
                '    def __class__(self):\n'
                '        raise KeyboardInterrupt("class hook")\n'
                'def cb():\n'
                '    raise Odd()\n',
                '<module>', 'exec'), scope)
            try:
                stub.call(scope['cb'])
            except asyncio.CancelledError as e:
                print('CancelledError', type(e).__name__)
            "#,
        ),
    );
    assert_eq!(out, "CancelledError Odd\n");
    assert_eq!(
        relay.output_in("b", 2, &with_root_in("b", "print(root.record(0, 'b'))")),
        "b\n"
    );
}

/// A callback's error that cannot be boxed -- a property RPyC reads that raises -- is answered
/// with an error that says what could not be sent, as the callback's error, and the connection
/// carries on rather than closing as for a frame it cannot decode.
#[test]
fn a_callbacks_error_that_cannot_be_boxed_is_answered_with_what_failed() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub_pool_in(
            "primary",
            r#"
            scope = {}
            exec(compile(
                'class Diagnosed(Exception):\n'
                '    @property\n'
                '    def detail(self):\n'
                '        raise ValueError("no detail")\n'
                'def cb():\n'
                '    raise Diagnosed("failed")\n',
                '<module>', 'exec'), scope)
            try:
                stub.call(scope['cb'])
            except Exception as e:
                print(type(e).__name__, e.args)
            print(numbers(), stub.method(1))
            "#,
        ),
    );
    assert_eq!(
        out,
        "RuntimeError ('a callback raised Diagnosed, which could not be sent to the host: \
         ValueError',)\n[1] 2\n"
    );
}

/// An interrupt whose signal lands once the primary's call waits for its reply -- after the reader
/// thread found the call busy and left the interrupt to the signal -- is raised from the wait,
/// which the handler leaves it on without waking: the wait looks again within `SIGNAL_CHECK`,
/// while the binding is still in the call. The signal is aimed and armed here, from a thread of
/// the execution's own, as the reader thread aims and arms one.
#[test]
fn an_interrupt_the_signal_leaves_on_a_waiting_call_is_raised_from_the_wait() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let release = Flag::new();
    let out = relay.output(
        1,
        &with_pool_in(
            "primary",
            &format!(
                r#"
                import __main__, signal
                main = threading.get_ident()
                def aim():
                    deadline = time.monotonic() + 20
                    while time.monotonic() < deadline:
                        wait = __main__._waits.get(main)
                        if wait is not None and wait.conn is not None and wait.waiting:
                            __main__._primary._interrupting = (1, False)
                            signal.pthread_kill(main, signal.SIGINT)
                            return
                        time.sleep(0.01)
                threading.Thread(target=aim, daemon=True).start()
                began = time.monotonic()
                try:
                    root.record(120, 'slow', until={release})
                except KeyboardInterrupt as e:
                    print('KeyboardInterrupt:', e)
                print(time.monotonic() - began < 10, pending(), numbers())
                "#,
                release = release.py()
            ),
        ),
    );
    assert_eq!(
        out,
        "KeyboardInterrupt: interrupted by outrig while waiting for binding 'fx' to answer: the \
         call's outcome on the host is unknown\nTrue [1] [1]\n"
    );
    release.raise();
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
    assert_eq!(interval(&recorded, "w2").thread, "serve-fx-primary-2");
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
    // The release, as `0003-30` will make it, from another thread.
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
    def counting(self, msg, seq, args, label=None):
        if msg == consts.MSG_REQUEST:
            counts[names.get(args[0], str(args[0]))] += 1
        return original(self, msg, seq, args, label)
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
        let rss_before = relay.binding_rss_kib();
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
                rss_during.push(relay.binding_rss_kib());
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
        let rss_after = relay.binding_rss_kib();
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
        let rss_before = (one.binding_rss_kib(), two.binding_rss_kib());
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
            |two| rss_during.push((one.binding_rss_kib(), two.binding_rss_kib())),
        );
        assert_eq!(result["status"], "ok", "{result}");
        let waited = one.result_from("primary", 2, TIMEOUT);
        assert_eq!(waited["output"], "done\n", "{waited}");
        let rss_after = (one.binding_rss_kib(), two.binding_rss_kib());
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
    let pid = nix::unistd::Pid::from_raw(relay.binding_pid() as i32);
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
    use crate::harness::bindings::{gather, lower};
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
        let here = std::env::current_dir().expect("a working directory");
        let factory = lower(&declared[0], &here).expect("lowered");
        let mut relay = Relay::start();
        relay.bind_factory(factory, fixture_dir());
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

// ---------------------------------------------------------------------------- the name is the proxy

/// Python that binds `K` to `agent`'s kernel and `stub` to its stub for binding `fx`, as the
/// session binds one under the binding's name, then runs `body`. Making it opens no connection
/// and sends nothing.
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

/// [`with_stub_in`], then [`POOL`] -- over the pool the stub made -- then `body`.
fn with_stub_pool_in(agent: &str, body: &str) -> String {
    with_stub_in(agent, &format!("{}\n{}", py(POOL), py(body)))
}

/// [`with_stub`], with the fixture's module importable here.
fn with_stub_and_fixture(body: &str) -> String {
    with_stub(&format!(
        "import sys\nsys.path.append({dir:?})\nimport outrig_fixture\n{body}",
        dir = fixture_dir().to_str().expect("a UTF-8 path"),
        body = py(body),
    ))
}

/// Python that counts the requests every connection sends, by handler, as [`COUNTING`] does, and
/// binds `measure(label, fn)`: it calls `fn` with the connection quiet before, and reports the
/// requests made meanwhile and every release of what it left, as
/// `{"requests": {handler: count}, "dels": count}`. The requests leave out releases and the
/// `inspect` RPyC sends to build a new proxy's class: bookkeeping, which the inventory does not
/// count as an operation.
const STUB_COUNTING: &str = r#"
    import collections, gc, json
    from rpyc.core import consts
    names = {v: n for n, v in vars(consts).items() if n.startswith('HANDLE_')}
    counts = collections.Counter()
    original = __main__._Hosted._send
    def counting(self, msg, seq, args, label=None):
        if msg == consts.MSG_REQUEST:
            counts[names.get(args[0], str(args[0]))] += 1
        return original(self, msg, seq, args, label)
    __main__._Hosted._send = counting
    report = {}
    def measure(label, fn):
        gc.collect()
        stub.public  # nothing from before is pending
        counts.clear()
        result = fn()
        ops = {
            name: n for name, n in counts.items() if name not in ('HANDLE_DEL', 'HANDLE_INSPECT')
        }
        dels = counts.get('HANDLE_DEL', 0)
        counts.clear()
        del result
        gc.collect()
        stub.public  # what the operation left is released before this request
        report[label] = {"requests": ops, "dels": dels + counts.get('HANDLE_DEL', 0)}
"#;

/// The lines `agent` has sent toward binding `fx`, as the relay counted them.
fn sent(relay: &Relay, agent: &str) -> usize {
    relay.stats.between(agent, "fx").to_binding.lines()
}

/// Making a stub sends nothing; its first use opens the kernel's first connection and fetches the
/// root there, on the thread using it, and the name is a `HostedRoot`.
#[test]
fn the_stub_fetches_nothing_until_it_is_used() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub("print(type(stub).__name__, type(stub).__module__, K._hosted['fx']._root)"),
    );
    assert_eq!(out, "HostedRoot outrig None\n");
    assert_eq!(sent(&relay, "primary"), 0);
    assert_eq!(relay.output(2, "print(stub.nested.value)"), "7\n");
    assert_eq!(ids_used(&relay, "primary"), [1]);
}

/// Each operation is the request RPyC's proxy sends, made when it runs: three `getattr` for
/// `a.b.c`; a `getattr`, a `call` and the bound method's release for a method call; `__iter__`
/// and a `__next__` per item, and one for the end, for `for`; and one request each for `len`,
/// truthiness, `in`, `==`, assignment, deletion, `repr`, `str`, `dir` and `hash`. A read that
/// fails -- missing, or refused -- is one request too. The host sees each effect.
#[test]
fn each_operation_is_the_request_rpycs_proxy_sends() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(&format!(
            "{}\n{}",
            py(STUB_COUNTING),
            py(r#"
                def with_block():
                    with stub.manager:
                        pass
                def for_loop():
                    items = []
                    for x in stub.sequence:
                        items.append(x)
                    return items
                measure('chain', lambda: stub.a.b.c)
                measure('method', lambda: stub.method(2, y=3))
                measure('for', for_loop)
                measure('len', lambda: len(stub.sequence))
                measure('bool', lambda: bool(stub))
                measure('in', lambda: 10 in stub.sequence)
                measure('eq', lambda: stub.a == 1)
                measure('set', lambda: setattr(stub, 'writable', 'after'))
                print(stub.writable)
                measure('delete', lambda: delattr(stub, 'writable'))
                measure('with', with_block)
                measure('repr', lambda: repr(stub))
                measure('str', lambda: str(stub))
                measure('dir', lambda: dir(stub))
                measure('hash', lambda: hash(stub))
                measure('missing', lambda: hasattr(stub, 'missing'))
                measure('private', lambda: hasattr(stub, '_private'))
                measure('class', lambda: isinstance(stub.nested, int))
                measure('close', lambda: stub.close())
                print(stub.method(2, y=3), [x for x in stub.sequence], len(stub.sequence),
                      10 in stub.sequence, stub.a == 1, hasattr(stub, 'writable'),
                      hasattr(stub, 'missing'), hasattr(stub, '_private'))
                print(stub.truth_tested, stub.sequence.iters, stub.sequence.nexts,
                      stub.manager.entered, stub.received)
                print(json.dumps(report))
            "#)
        )),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "after", "{out}");
    assert_eq!(
        lines[1], "5 [10, 20, 30] 3 True False False False False",
        "{out}"
    );
    assert_eq!(lines[2], "1 2 8 1 ['close']", "{out}");
    let report: Value = serde_json::from_str(lines[3]).expect("a JSON report");
    let expected = [
        ("chain", json!({"HANDLE_GETATTR": 3})),
        ("method", json!({"HANDLE_GETATTR": 1, "HANDLE_CALL": 1})),
        ("for", json!({"HANDLE_GETATTR": 1, "HANDLE_CALLATTR": 5})),
        ("len", json!({"HANDLE_GETATTR": 1, "HANDLE_CALLATTR": 1})),
        ("bool", json!({"HANDLE_CALLATTR": 1})),
        ("in", json!({"HANDLE_GETATTR": 1, "HANDLE_CALLATTR": 1})),
        ("eq", json!({"HANDLE_GETATTR": 1, "HANDLE_CMP": 1})),
        ("set", json!({"HANDLE_SETATTR": 1})),
        ("delete", json!({"HANDLE_DELATTR": 1})),
        (
            "with",
            json!({"HANDLE_GETATTR": 1, "HANDLE_CALLATTR": 1, "HANDLE_CTXEXIT": 1}),
        ),
        ("repr", json!({"HANDLE_REPR": 1})),
        ("str", json!({"HANDLE_STR": 1})),
        ("dir", json!({"HANDLE_DIR": 1})),
        ("hash", json!({"HANDLE_HASH": 1})),
        ("missing", json!({"HANDLE_GETATTR": 1})),
        ("private", json!({"HANDLE_GETATTR": 1})),
        // `nested`, then its `__class__`: no local class names the host's here, so it is asked,
        // and refused, once.
        ("class", json!({"HANDLE_GETATTR": 2})),
        ("close", json!({"HANDLE_GETATTR": 1, "HANDLE_CALL": 1})),
    ];
    for (op, requests) in expected {
        assert_eq!(report[op]["requests"], requests, "{op}: {report}");
    }
    assert_eq!(report["method"]["dels"], 1, "{report}");
}

/// What a call gives back is a proxy used the same way, or a value; a tuple comes back as a
/// tuple. `==` asks the host. With its library imported here before it first arrives, a proxy's
/// class is the library's for `isinstance`. Nothing is awaitable, and the inventory names the
/// host's type; `help` of the stub describes the root, not the stub's class.
#[test]
fn a_result_is_a_proxy_used_the_same_way() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub_and_fixture(&format!(
            "{}\n{}",
            py(STUB_COUNTING),
            py(r#"
                nested = stub.nested
                print(type(nested).__name__, nested.value, nested.method())
                pair = stub.pair()
                print(type(pair).__name__, len(pair), type(pair[0]).__name__, pair[1])
                measure('eq', lambda: stub.nested == stub.nested)
                print(stub.nested == stub.nested, report['eq']['requests'].get('HANDLE_CMP'))
                print(isinstance(nested, outrig_fixture.Nested), nested.__class__.__name__)
                for label, target in (('a proxy', stub.a), ('the stub', stub)):
                    try:
                        await target
                    except TypeError:
                        print(label, 'is not awaitable')
                names = runtime.names()
                print(names['nested'], names['pair'], names['stub'])
            "#)
        )),
    );
    assert_eq!(
        out,
        "outrig_fixture.Nested 7 nested method\n\
         tuple 2 outrig_fixture.Nested 1\n\
         True 1\n\
         True Nested\n\
         a proxy is not awaitable\n\
         the stub is not awaitable\n\
         outrig_fixture.Nested tuple HostedRoot\n"
    );
    let help = relay.output(2, "help(stub)");
    assert!(help.contains("outrig_fixture.Root"), "{help}");
    assert!(!help.contains("HostedRoot"), "{help}");
}

/// The stub and its proxies pass as arguments, as themselves: the stub as the root it stands for,
/// a proxy inside a list or as a dict key too, and a proxied list as a reference, not a list to
/// copy. The host's method runs once for each call.
#[test]
fn the_stub_and_its_proxies_pass_as_arguments() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            before = stub.taken
            print(stub.takes(stub.nested), stub.takes(stub), stub.taken - before)
            print(stub.echo(stub) == stub)
            print(stub.shape([stub.nested, {'k': stub.nested}]))
            print(stub.shape({stub.nested: 1}))
            records = stub.list_records(2)
            print(stub.takes(records), stub.taken - before)
            "#,
        ),
    );
    assert_eq!(
        out,
        "True False 2\nTrue\nlist[Nested, {str: Nested}]\n{Nested: int}\nFalse 3\n"
    );
}

/// A hosted object belongs to the kernel whose pool made it: another kernel can neither use it
/// nor pass it to the binding, and the host's method is not called.
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
            for label, fn in (('use', lambda: __main__.shared.value),
                              ('pass', lambda: stub.takes(__main__.shared))):
                try:
                    fn()
                except TypeError as e:
                    print(label, '->', e)
            print(stub.taken)
            "#,
        ),
    );
    assert_eq!(
        out,
        "use -> a hosted object of agent 'primary' cannot be used by agent 'b': each agent \
         reaches a binding through its own\n\
         pass -> a proxy of binding 'fx' on agent 'primary' cannot be passed to binding 'fx' on \
         agent 'b': a hosted object crosses only to its own binding, from the kernel that holds \
         it\n\
         0\n"
    );
}

/// A hosted exception is raised where the call is made, as the library's own class once its
/// module imports here, with its public attributes. The traceback an execution reports shows the
/// agent's code and none of RPyC's frames or this program's, for the exception, for one chained to
/// it and for one in a group -- and the exception agent code keeps still has them all.
#[test]
fn a_hosted_exception_is_raised_typed_where_the_call_is_made() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub_and_fixture(
            r#"
            try:
                stub.raise_fixture()
            except outrig_fixture.FixtureError as e:
                print(e.args, e.status)
            "#,
        ),
    );
    assert_eq!(out, "('fixture failed',) 3\n");
    for (id, source, raised) in [
        (2, "stub.raise_fixture()", "FixtureError"),
        (
            3,
            "try:\n    stub.missing\nexcept AttributeError:\n    raise ValueError('chained')",
            "ValueError",
        ),
        (4, "len(stub)", "TypeError"),
        (
            5,
            "import builtins\ntry:\n    stub.raise_fixture()\nexcept Exception as e:\n    \
             builtins.kept = e\n    raise ExceptionGroup('grouped', [e])",
            "ExceptionGroup",
        ),
    ] {
        let result = relay.exec(id, source);
        assert_eq!(result["raised"], raised, "{result}");
        let error = result["error"].as_str().expect("an error");
        assert!(error.contains("File \"<execution>\""), "{error}");
        for machinery in ["rpyc", "netref", "File \"<string>\"", "sync_request"] {
            assert!(!error.contains(machinery), "{machinery} in {error}");
        }
    }
    let frames = relay.output(
        6,
        "import builtins, traceback\n\
         print(any('rpyc' in f.filename for f in traceback.extract_tb(builtins.kept.__traceback__)))",
    );
    assert_eq!(frames, "True\n");
}

/// A binding's name is an instance of the class its root's `root_type` names, as the root's own
/// proxy is, and of no other, though the module that defines the class is not one this process
/// imports.
#[test]
fn a_binding_is_an_instance_of_the_class_its_member_names() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            print(isinstance(stub, stub.root_type), isinstance(stub, stub.nested_type),
                  isinstance(stub, (int, stub.root_type)), isinstance(stub.nested, stub.nested_type))
            "#,
        ),
    );
    assert_eq!(out, "True False True True\n");
}

/// A hosted exception whose class refuses to have its traceback set -- here a read-only
/// `__traceback__` -- is still reported, without RPyC's frames or this program's: they are left
/// out through `BaseException`'s own descriptor, and put back the same way.
#[test]
fn a_hosted_exception_that_refuses_a_traceback_is_still_reported_without_hosted_frames() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let result = relay.exec(
        1,
        &with_stub(
            r#"
            import sys, types
            scope = {}
            exec(compile(
                'class FixtureError(Exception):\n'
                '    @property\n'
                '    def __traceback__(self):\n'
                '        return None\n',
                '<module>', 'exec'), scope)
            module = types.ModuleType('outrig_fixture')
            module.FixtureError = scope['FixtureError']
            sys.modules['outrig_fixture'] = module
            stub.raise_fixture()
            "#,
        ),
    );
    assert_eq!(result["raised"], "FixtureError", "{result}");
    let error = result["error"].as_str().expect("an error");
    assert!(error.contains("File \"<execution>\""), "{error}");
    assert!(!error.contains("could not be formatted"), "{error}");
    for machinery in ["rpyc", "netref", "sync_request"] {
        assert!(!error.contains(machinery), "{machinery} in {error}");
    }
}

/// `hasattr` asks the host, and `getattr` with a default takes it when the host says no: a missing
/// attribute raises the library's `AttributeError`, and the binding's refusal of a private one an
/// error of OutRig's that is an `AttributeError` too, which the library's own classes do not
/// catch. A refused assignment, and a read refused for what it would return, stay OutRig's plain
/// refusal.
#[test]
fn a_missing_or_refused_attribute_is_an_attribute_error() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub_and_fixture(
            r#"
            print(hasattr(stub, 'missing'), hasattr(stub, '_private'),
                  getattr(stub, '_private', 'default'))
            for name in ('missing', '_private'):
                try:
                    getattr(stub, name)
                except (outrig_fixture.FixtureError, outrig_fixture.CommandError):
                    print('the library caught it')
                except AttributeError as e:
                    print(name, type(e).__name__, e.name, str(e).splitlines()[0])
            def refused(label, fn):
                try:
                    fn()
                except AttributeError:
                    print(label, 'an AttributeError')
                except Exception as e:
                    print(label, type(e).__name__)
            refused('set', lambda: setattr(stub, '_private', 1))
            g = stub.gen()
            refused('result', lambda: g.gi_frame)
            "#,
        ),
    );
    assert_eq!(
        out,
        "False False default\n\
         missing AttributeError missing 'Root' object has no attribute 'missing'\n\
         _private outrig.Refused _private '_private' is not a public name, and it is not on \
         RPyC's safe list\n\
         set outrig.Refused\n\
         result outrig.Refused\n"
    );
}

/// RPyC rebuilds the binding's refusal as `outrig.Refused` where the module `outrig` has a class
/// `Refused` -- one agent code put in `sys.modules` in place of OutRig's own package, say -- rather
/// than as its own class of that name: the refusal of a name is an `AttributeError` either way.
#[test]
fn a_refusal_rebuilt_from_a_local_outrig_module_is_an_attribute_error_too() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import sys, types
            package = sys.modules['outrig']
            module = sys.modules['outrig'] = types.ModuleType('outrig')
            exec('class Refused(Exception):\n    pass\n', module.__dict__)
            try:
                print(hasattr(stub, '_private'), getattr(stub, '_private', 'default'))
                try:
                    stub._private
                except AttributeError as e:
                    print(type(e).__name__, isinstance(e, module.Refused), e.name)
                try:
                    stub._private = 1
                except module.Refused as e:
                    print('set', isinstance(e, AttributeError))
            finally:
                sys.modules['outrig'] = package
            "#,
        ),
    );
    assert_eq!(out, "False default\nRefused True _private\nset False\n");
}

/// `with` enters the host's object, and an exception raised inside reaches its `__exit__` by
/// value; whether it is suppressed is the truth of what `__exit__` gives back -- a list, which
/// crosses by reference, asks the host -- tested only when the body raised, as `with` does.
#[test]
fn with_exits_and_suppresses_as_on_the_host() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            with stub.manager as m:
                print(m.entered)
            try:
                with stub.manager:
                    raise ValueError('inside', 5)
            except ValueError as e:
                print('raised', e.args, stub.manager.exits[-1])
            for name in ('suppressing', 'propagating'):
                try:
                    with getattr(stub, name):
                        raise ValueError(name)
                    print(name, 'suppressed')
                except ValueError as e:
                    print(name, 'raised', e)
            with stub.untestable:
                pass
            print('clean')
            try:
                with stub.untestable:
                    raise KeyError('body')
            except Exception as e:
                print('raised', type(e).__name__)
            "#,
        ),
    );
    assert_eq!(
        out,
        "1\n\
         raised ('inside', 5) ('ValueError', \"ValueError('inside', 5)\", True)\n\
         suppressing suppressed\n\
         propagating raised propagating\n\
         clean\n\
         raised ValueError\n"
    );
}

/// A 10 s call on the kernel's thread holds that kernel's loop: a timer task does not fire and a
/// `runtime.wait` in another task does not return on the user's message until the call returns,
/// while a second kernel runs executions and answers `inv` throughout.
#[test]
fn a_call_on_the_kernels_thread_holds_its_loop() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    let (started, release) = (Flag::new(), Flag::new());
    relay.submit_in(
        "primary",
        1,
        &with_stub(&format!(
            r#"
            import time
            marks = {{}}
            async def timer():
                await asyncio.sleep(0.1)
                marks['timer'] = time.monotonic()
            async def waiter():
                try:
                    await runtime.wait({{asyncio.get_running_loop().create_future()}})
                except runtime.MessageAvailable:
                    marks['message'] = time.monotonic()
            tasks = [asyncio.create_task(timer()), asyncio.create_task(waiter())]
            await asyncio.sleep(0)
            print(stub.record(10, 'held', started={started}, until={release}))
            returned = time.monotonic()
            await asyncio.gather(*tasks)
            print(marks['timer'] > returned, marks['message'] > returned,
                  (await runtime.channels['user'].receive()).body)
            "#,
            started = started.py(),
            release = release.py(),
        )),
    );
    started.wait();
    assert_eq!(
        relay.output_in("b", 1, &with_stub_in("b", "print(stub.method(1))")),
        "2\n"
    );
    assert!(relay.inv("b", 2)["globals"].is_array());
    assert_eq!(relay.msg("primary", 3, "user", "hello")["pending"], 1);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        relay.output_in("b", 4, &with_stub_in("b", "print(stub.public)")),
        "public value\n"
    );
    release.raise();
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["output"], "held\nTrue True hello\n", "{result}");
}

/// The same call made with `asyncio.to_thread` leaves the loop turning: a timer fires and
/// `runtime.wait` returns on the user's message while the call runs, and a call the loop's own
/// thread makes meanwhile takes a second connection.
#[test]
fn a_call_in_a_worker_leaves_the_loop_turning() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (started, heard, release) = (Flag::new(), Flag::new(), Flag::new());
    relay.submit_in(
        "primary",
        1,
        &with_stub_pool_in(
            "primary",
            &format!(
                r#"
                call = asyncio.create_task(asyncio.to_thread(
                    lambda: stub.record(10, 'held', started={started}, until={release})
                ))
                await appears({started})
                print(await asyncio.sleep(0.1, result='fired'), call.done())
                try:
                    await runtime.wait({{call}})
                except runtime.MessageAvailable:
                    print('message', (await runtime.channels['user'].receive()).body, call.done())
                print(stub.public, numbers())
                touch({heard})
                print(await call)
                "#,
                started = started.py(),
                release = release.py(),
                heard = heard.py(),
            ),
        ),
    );
    started.wait();
    relay.msg("primary", 2, "user", "hello");
    heard.wait();
    release.raise();
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(
        result["output"], "fired False\nmessage hello False\npublic value [1, 2]\nheld\n",
        "{result}"
    );
}

/// Four calls from one kernel, each in a worker, take four connections, and a fifth waits for
/// one, opening none, and runs when one of the four returns.
#[test]
fn four_calls_take_four_connections_and_a_fifth_waits() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub_pool_in(
            "primary",
            r#"
            import tempfile
            d = tempfile.mkdtemp()
            starts = [os.path.join(d, f's{i}') for i in range(5)]
            ends = [os.path.join(d, f'e{i}') for i in range(4)]
            def record(i, until=None):
                return lambda: stub.record(0, f'c{i}', started=starts[i], until=until)
            calls = [asyncio.create_task(asyncio.to_thread(record(i, ends[i]))) for i in range(4)]
            for path in starts[:4]:
                await appears(path)
            fifth = asyncio.create_task(asyncio.to_thread(record(4)))
            await asyncio.sleep(0.5)
            print(os.path.exists(starts[4]), numbers())
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
        "False [1, 2, 3, 4]\nc0 c4 [1, 2, 3, 4]\n['c1', 'c2', 'c3']\n"
    );
    assert_eq!(ids_used(&relay, "primary"), [1, 2, 3, 4]);
}

/// A coroutine function is refused as a callback before the call is sent, whether the call is
/// made on the kernel's thread or in a worker: the host calls a callback as a plain function and
/// never awaits it. The fixture's method is never called.
#[test]
fn a_coroutine_callback_is_refused_before_the_call_is_sent() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import functools
            async def coroutine():
                return 1
            class Holder:
                async def method(self):
                    return 1
            for label, call in (
                ('thread', lambda: stub.noted(coroutine)),
                ('partial', lambda: stub.noted(functools.partial(coroutine))),
                ('method', lambda: stub.noted(Holder().method)),
            ):
                try:
                    call()
                except TypeError as e:
                    print(label, '->', e)
            try:
                await asyncio.to_thread(lambda: stub.noted(coroutine))
            except TypeError as e:
                print('worker ->', e)
            print(stub.calls)
            "#,
        ),
    );
    let refusal = "a coroutine function cannot be a hosted object's callback: the host calls a \
                   callback as a plain function and never awaits what it returns. Pass a plain \
                   function.";
    assert_eq!(
        out,
        format!(
            "thread -> {refusal}\npartial -> {refusal}\nmethod -> {refusal}\n\
             worker -> {refusal}\n0\n"
        )
    );
}

/// A plain callback that returns a coroutine is refused when the host calls it: the coroutine is
/// closed, never left unawaited, and the host's call of it raises.
#[test]
fn a_callback_returning_a_coroutine_is_refused_when_called() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import gc
            async def coroutine():
                return 1
            print(stub.calling_raised(lambda: coroutine()))
            collected = gc.collect()
            "#,
        ),
    );
    assert_eq!(out, "TypeError\n");
    assert!(
        !relay.stderr().contains("never awaited"),
        "{}",
        relay.stderr()
    );
}

/// A plain callback runs on the thread serving its call, in the calling execution's context --
/// the kernel's thread for a call made there, the worker for one made in `asyncio.to_thread` --
/// and its output is that execution's.
#[test]
fn a_plain_callback_runs_on_the_thread_serving_its_call() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import contextvars, threading
            cv = contextvars.ContextVar('cv')
            cv.set('the calling execution')
            def callback(nested):
                print('in the callback')
                return threading.current_thread().name, cv.get(), nested.value
            print(stub.call_with_nested(callback))
            def in_worker():
                me = threading.current_thread()
                return stub.call(lambda: (threading.current_thread() is me, cv.get()))
            print(await asyncio.to_thread(in_worker))
            "#,
        ),
    );
    assert_eq!(
        out,
        "in the callback\n('MainThread', 'the calling execution', 7)\n\
         (True, 'the calling execution')\n"
    );
}

/// A callback of a hosted call runs in the calling execution's context, so
/// `outrig.runtime` there is the calling kernel's runtime -- on the kernel's
/// thread and in a `to_thread` worker, in the first kernel and a later one.
#[test]
fn outrig_runtime_in_a_callback_is_the_calling_executions() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("b");
    for (agent, id) in [("primary", 1), ("b", 1)] {
        let out = relay.output_in(
            agent,
            id,
            &with_stub_in(
                agent,
                r#"
                import outrig
                def callback(nested):
                    return outrig.runtime.history is runtime.history is K.runtime.history
                print(stub.call_with_nested(callback))
                def in_worker():
                    return stub.call(lambda: outrig.runtime.history is K.runtime.history)
                print(await asyncio.to_thread(in_worker))
                "#,
            ),
        );
        assert_eq!(out, "True\nTrue\n", "{agent}");
    }
}

/// The release of a kernel -- `close_hosted`, from another thread -- while a call on its thread
/// and a call in a worker are in flight: each raises `EOFError` saying its outcome is unknown,
/// and the binding finishes both.
#[test]
fn closing_the_pool_ends_both_calls_unknown() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("c");
    let (in_worker, on_thread) = (Flag::new(), Flag::new());
    relay.submit_in(
        "c",
        1,
        &with_stub_pool_in(
            "c",
            &format!(
                r#"
                import tempfile
                release = os.path.join(tempfile.mkdtemp(), 'release')
                worker = asyncio.create_task(asyncio.to_thread(
                    lambda: stub.record(0, 'worker', started={in_worker}, until=release)
                ))
                await appears({in_worker})
                ended = []
                try:
                    stub.record(0, 'thread', started={on_thread}, until=release)
                except EOFError as e:
                    ended.append(f'thread: {{e}}')
                try:
                    await worker
                except EOFError as e:
                    ended.append(f'worker: {{e}}')
                print('\n'.join(ended))
                touch(release)
                "#,
                in_worker = in_worker.py(),
                on_thread = on_thread.py(),
            ),
        ),
    );
    on_thread.wait();
    relay.output(2, "import __main__\n__main__._kernels['c'].close_hosted()");
    let result = relay.result_from("c", 1, TIMEOUT);
    let unknown = "the kernel's connections are closing: the call's outcome on the host is unknown";
    assert_eq!(
        result["output"],
        format!("thread: {unknown}\nworker: {unknown}\n"),
        "{result}"
    );
    eventually(
        || {
            let recorded = intervals(&mut relay, "primary", 3);
            (["thread", "worker"]
                .iter()
                .all(|label| recorded.iter().any(|i| &i.label == label)))
            .then_some(())
        },
        || "the two calls to finish on the host".to_string(),
    );
}

/// No name a kernel boots resolves a hosted object through an awaitable: the stub bound for each
/// binding is no awaitable, nor any public attribute of a name bound at boot, and the awaited
/// facade is gone. Checked on the types, so nothing asks the host.
#[test]
fn no_name_a_kernel_boots_resolves_a_hosted_object_through_an_awaitable() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.declare_bindings();
    relay.open("b");
    let probe = r#"
        import __main__, inspect
        K = __main__._kernels[AGENT]
        hosted = (__main__.HostedRoot, __main__._rpyc.core.netref.BaseNetref)
        def awaitable(t):
            return any(hasattr(t, name) for name in ('__await__', '__aiter__', '__aenter__'))
        stubs, found = [], []
        for name, value in K._boot.items():
            if type(value) is __main__.HostedRoot:
                stubs.append(name)
            if issubclass(type(value), hosted):
                if awaitable(type(value)):
                    found.append(name)
                continue
            if name == '__builtins__' or inspect.ismodule(value):
                continue
            for attr in dir(value):
                if attr.startswith('_'):
                    continue
                try:
                    member = getattr(value, attr)
                except Exception:
                    continue
                if issubclass(type(member), hosted):
                    found.append(f'{name}.{attr}')
        print(stubs, found, hasattr(__main__, 'HostedObject'))
    "#;
    for agent in ["primary", "b"] {
        assert_eq!(
            relay.output_in(agent, 1, &format!("AGENT = {agent:?}\n{}", py(probe))),
            "['fx'] [] False\n",
            "{agent}"
        );
    }
    assert_eq!(sent(&relay, "primary"), 0);
}
