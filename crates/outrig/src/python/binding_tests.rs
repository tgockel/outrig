//! The hosted-object transport, end to end on the host: an interpreter and binding processes
//! started by [`Relay`], the fixture library in each binding, and every request checked in the
//! binding.
//!
//! Nothing on the container side is enforcement, so the tests of a client written by hand send
//! RPyC requests past the interpreter's shim -- as agent code could, since it runs in the same
//! process as the transport's container end -- and expect the binding to refuse them with the
//! target untouched.

use std::process::Command;

use serde_json::{Value, json};

use super::interpreter_tests::Interpreter;
use super::relay::{BINDING_SOURCE, Relay, fixture_dir, rpyc_dir};
use super::testing::{Start, py, python};

// The programs' bounds, restated so that changing one changes a test.
const PART_MAX: usize = 512 * 1024;
const FRAME_MAX: usize = 128 << 20;
/// The longest `rpc` line either way: one part, base64-encoded, inside its JSON line.
const RPC_LINE_MAX: usize = 1 << 20;

const FACTORY: &str = "outrig_fixture:make";

/// Python that binds `root` to the primary kernel's proxy of binding `fx`, then runs `body`.
fn with_root(body: &str) -> String {
    format!(
        "import __main__\nroot = __main__._kernels['primary'].hosted('fx')\n{}",
        py(body)
    )
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
    def raw(handler, boxed):
        res = rpyc.core.async_.AsyncResult(conn)
        seq = conn._get_seq_id()
        conn._request_callbacks[seq] = res
        conn._send(consts.MSG_REQUEST, seq, (handler, boxed))
        return res.value
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
