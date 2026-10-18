"""The fixture library a binding process hosts in tests."""

import itertools
import os
import pickle
import signal
import subprocess
import sys
import threading
import time
import types
import weakref


class Records:
    """The service shape: a wait that blocks until someone answers, a question asked for a ticket
    and polled for, and records in bulk. One implementation of the state, used in the binding's
    process by `Root` and in the service process the two-session measurement starts."""

    def __init__(self):
        self._answer = threading.Event()
        self._answer_value = None
        self._tickets = {}  # ticket -> [question, polls so far]
        self._next_ticket = itertools.count(1)
        self._records_lock = threading.Lock()
        self._records = [
            {"id": i, "name": f"record {i}", "score": i * 1.5, "flag": bool(i % 2)}
            for i in range(1000)
        ]

    def wait_for_answer(self, until=None, timeout=600):
        """Block until `answer` is called -- or, given `until`, until that file exists: the one
        release that works inside a binding started with `--serialize`, where `answer` would
        wait for the lock this call holds."""
        deadline = time.monotonic() + timeout
        while True:
            if self._answer.wait(0.05):
                return self._answer_value
            if until is not None and os.path.exists(until):
                return "released by file"
            if time.monotonic() > deadline:
                raise TimeoutError("nobody answered")

    def answer(self, value):
        self._answer_value = value
        self._answer.set()

    def ask(self, question):
        """A ticket for `question`, answered on the second poll."""
        ticket = next(self._next_ticket)
        self._tickets[ticket] = [question, 0]
        return ticket

    def poll(self, ticket):
        entry = self._tickets[ticket]
        entry[1] += 1
        return f"answer to {entry[0]}" if entry[1] >= 2 else None

    def list_records(self, n):
        """The first `n` records as a list of dicts, which the agent receives proxied."""
        with self._records_lock:
            return [dict(record) for record in self._records[:n]]

    def records_by_value(self, n):
        """The first `n` records as field names and a tuple of tuples, which cross by value."""
        with self._records_lock:
            rows = tuple(
                (record["id"], record["name"], record["score"], record["flag"])
                for record in self._records[:n]
            )
        return ("id", "name", "score", "flag"), rows

    def update_records(self, changes):
        """Apply `changes`, dicts with an `id`, and return how many."""
        with self._records_lock:
            by_id = {record["id"]: record for record in self._records}
            for change in changes:
                by_id[change["id"]].update(change)
        return len(changes)


class ServiceClient:
    """A client of the service process, as a Rust service would ship one: each method is one
    round trip over a Unix socket. One connection per calling thread, since the binding serves
    its connections on several threads and one socket cannot carry two exchanges at once."""

    def __init__(self, address):
        self._address = address
        self._local = threading.local()

    def _connection(self):
        conn = getattr(self._local, "conn", None)
        if conn is None:
            from multiprocessing.connection import Client

            conn = self._local.conn = Client(self._address, family="AF_UNIX", authkey=b"fixture")
        return conn

    def _call(self, method, *args, **kwargs):
        conn = self._connection()
        conn.send((method, args, kwargs))
        status, value = conn.recv()
        if status == "error":
            raise RuntimeError(value)
        return value

    def wait_for_answer(self, until=None, timeout=600):
        return self._call("wait_for_answer", until=until, timeout=timeout)

    def answer(self, value):
        return self._call("answer", value)

    def ask(self, question):
        return self._call("ask", question)

    def poll(self, ticket):
        return self._call("poll", ticket)

    def list_records(self, n):
        return self._call("list_records", n)

    def records_by_value(self, n):
        return self._call("records_by_value", n)

    def update_records(self, changes):
        return self._call("update_records", changes)


def service_client():
    """The factory of a binding that is a client of the service process."""
    return ServiceClient(os.environ["OUTRIG_FIXTURE_SERVICE"])


class Fresh:
    """An object made on request, so a test can watch the binding's table drop it."""

    def __init__(self, root, name):
        self._root = root
        self.name = name

    def record(self, **kwargs):
        return self._root.record(**kwargs)


class FixtureError(Exception):
    """Carries a public attribute beside its args."""

    def __init__(self, message, status):
        super().__init__(message)
        self.status = status


class CommandError(Exception):
    """Reads private state in __str__, as GitPython's GitCommandError reads _cmdline."""

    def __init__(self, command, status):
        super().__init__(command, status)
        self._cmdline = " ".join(command)
        self.status = status

    def __str__(self):
        return f"command {self._cmdline!r} failed with status {self.status}"


class Nested:
    def __init__(self):
        self.value = 7
        self._secret = "hidden"

    def method(self):
        return "nested method"


class Manager:
    """A context manager whose `__exit__` gives back `result`: a list crosses by reference."""

    def __init__(self, result=False):
        self.entered = 0
        self.exits = []
        self.result = result

    def __enter__(self):
        self.entered += 1
        return self

    def __exit__(self, typ, value, tb):
        self.exits.append((None if typ is None else typ.__name__, repr(value), tb is None))
        return self.result


class Untestable:
    """What an `__exit__` may give back: testing its truth raises."""

    def __bool__(self):
        raise ValueError("its truth cannot be told")


class Counted(list):
    """A list that counts, on the host, the iterations it starts and the items they take."""

    def __init__(self, items):
        super().__init__(items)
        self.iters = 0
        self.nexts = 0

    def __iter__(self):
        self.iters += 1
        return CountingIterator(self, list.__iter__(self))


class CountingIterator:
    def __init__(self, owner, iterator):
        self._owner = owner
        self._iterator = iterator

    def __iter__(self):
        return self

    def __next__(self):
        self._owner.nexts += 1
        return next(self._iterator)


class Callable:
    def __call__(self, x):
        return x * 2

    def public(self):
        return 1

    def _private(self):
        return 2


unpickled = 0
_real_loads = pickle.loads


def _counting_loads(data, *args, **kwargs):
    global unpickled
    unpickled += 1
    return _real_loads(data, *args, **kwargs)


pickle.loads = _counting_loads


class Marked:
    """An object whose `repr` and `str` each write a file beside `base`: code anything that
    rendered it would run."""

    def __init__(self, base):
        self.base = base

    def __repr__(self):
        open(f"{self.base}.repr", "w").close()
        return "Marked()"

    def __str__(self):
        open(f"{self.base}.str", "w").close()
        return "Marked"


class Root(Records):
    def __init__(self):
        Records.__init__(self)
        self.public = "public value"
        self._private = "private value"
        self.writable = "before"
        self.nested = Nested()
        self.sequence = Counted([10, 20, 30])
        self.manager = Manager()
        self.suppressing = Manager(["suppress"])
        self.propagating = Manager([])
        self.untestable = Manager(Untestable())
        self.callable = Callable()
        self.kept = None
        self.nested_type = Nested
        self.str_type = str
        self.root_type = Root
        self.a = types.SimpleNamespace(b=types.SimpleNamespace(c="c"))
        self.b = "b"
        self.calls = 0
        self.taken = 0
        self.truth_tested = 0
        self.received = []
        self.last_raised = None
        self._lock = threading.Lock()
        self._intervals = []
        self._kept = {}  # name -> (weak reference, strong reference or None)

    def _private_method(self):
        return "private"

    def __bool__(self):
        """Counted, so a test sees that `bool()` of the root asked the host."""
        self.truth_tested += 1
        return True

    def record(self, seconds=0, label="", started=None, until=None):
        """The recording method: notes when it starts and ends here, on the host, and the thread
        that served it. It sleeps `seconds`, or, given `until`, waits for that file to exist with
        `seconds or 60` as the ceiling; `started` is a file it creates on starting, so a test
        knows the call is in flight."""
        start = time.monotonic()
        if started:
            open(started, "w").close()
        if until is None:
            time.sleep(seconds)
        else:
            deadline = start + (seconds or 60)
            while not os.path.exists(until):
                if time.monotonic() > deadline:
                    raise TimeoutError(f"{until} never appeared")
                time.sleep(0.01)
        self._note(label, start)
        return label

    def hold_callable(self, fn, started=None, until=None):
        """Wait as `record` does, holding `fn` without calling it: a call that carries a callable
        and blocks."""
        self.record(0, "hold", started=started, until=until)

    def call_then_fresh(self, fn, name):
        """Call `fn`, swallow what it raises -- a connection that closed under it -- and return a
        fresh object under `name`, as a library that survives a failed callback and still
        answers would."""
        try:
            fn()
        except Exception:
            pass
        return self.fresh(name)

    def call_recorded(self, fn, label):
        """`fn()`, with its interval recorded as `record` records one."""
        start = time.monotonic()
        try:
            return fn()
        finally:
            self._note(label, start)

    def _note(self, label, start):
        with self._lock:
            self._intervals.append((label, start, time.monotonic(), threading.current_thread().name))

    def intervals(self):
        """Every recorded interval as `(label, start, end, thread name)`, by value."""
        with self._lock:
            return tuple(self._intervals)

    def thread_name(self):
        return threading.current_thread().name

    def fresh(self, name):
        """A new object, kept here under `name` with a weak reference beside it."""
        obj = Fresh(self, name)
        self._kept[name] = (weakref.ref(obj), obj)
        return obj

    def again(self, name):
        """The object `fresh` made under `name`, once more."""
        return self._kept[name][1]

    def let_go(self, name):
        """Drop this module's own reference to `name`'s object, so the binding's table alone
        holds it."""
        ref, _ = self._kept[name]
        self._kept[name] = (ref, None)

    def alive(self, name):
        """Whether `name`'s object still exists, after a collection."""
        import gc

        gc.collect()
        return self._kept[name][0]() is not None

    def sleep_child(self, seconds, started=None):
        """Run `sleep seconds` as a child of this process and wait for it: a call in flight whose
        grandchild the owner must end with the binding. `started` is a file created once the
        child runs."""
        child = subprocess.Popen(["sleep", str(seconds)])
        if started:
            open(started, "w").close()
        return child.wait()

    def escape(self, pidfile, seconds):
        """Fork a child that leaves this process's group with `setsid`, writes its pid to
        `pidfile`, and sleeps a minute holding every descriptor this process had -- the
        protocol's output among them, which stopping the group then leaves open -- and sleep
        `seconds`: a call in flight whose library's helper outlives the binding."""
        if os.fork() == 0:
            os.setsid()
            with open(f"{pidfile}.part", "w") as f:
                f.write(str(os.getpid()))
            os.rename(f"{pidfile}.part", pidfile)
            time.sleep(60)
            os._exit(0)
        time.sleep(seconds)

    def event(self, payload):
        """Publish `payload` as an event line, through the binding program's own primitive."""
        import __main__

        __main__._event(payload)

    def event_path(self, raw):
        """`event` with a path the host made from the bytes `raw`, as a library reports a file
        name: not UTF-8 when the bytes are not."""
        import __main__

        __main__._event({"path": os.fsdecode(raw)})

    def env(self, name):
        return os.environ.get(name)

    def child_env(self, name):
        """`name` as a program this process starts sees it."""
        run = subprocess.run(["sh", "-c", f"echo ${name}"], capture_output=True, text=True)
        return run.stdout.strip()

    def sys_path(self):
        return tuple(sys.path)

    def pgid(self):
        return os.getpgrp()

    def method(self, x, y=1):
        return x + y

    def takes(self, obj):
        self.taken += 1
        return obj is self.nested

    def pair(self):
        return (self.nested, 1)

    def slow(self, seconds=0.2, label="slow"):
        return self.record(seconds, label)

    def close(self):
        """A public name a coroutine would own, recorded when it is called."""
        self.received.append("close")
        return "closed"

    def send(self, value):
        """A public name a coroutine would own, recorded when it is called."""
        self.received.append(("send", value))
        return value

    def iterable(self):
        """A generator, as a library returns one."""
        return (n for n in (1, 2, 3))

    def echo(self, x):
        """`x` itself, so a callable passed in comes back as a reference to it."""
        return x

    def types(self, *args, **kwargs):
        return tuple(type(a).__name__ for a in args)

    def shape(self, obj):
        """The types of a nested value, as text."""
        if isinstance(obj, dict):
            items = sorted(obj.items(), key=repr)
            return "{" + ", ".join(f"{self.shape(k)}: {self.shape(v)}" for k, v in items) + "}"
        if isinstance(obj, (list, tuple, set, frozenset)):
            return type(obj).__name__ + "[" + ", ".join(sorted(self.shape(i) for i in obj)) + "]"
        return type(obj).__name__

    def noted(self, *args):
        """Counts its calls, so a test can show a refused request never reached it."""
        self.calls += 1
        return self.calls

    def call(self, fn, *args):
        return fn(*args)

    def call_in_thread(self, fn, *args):
        """`fn(*args)` on a thread of its own, joined, as a library that calls back from a worker
        does: the callback's own requests are served away from the thread this call holds."""
        box = {}

        def run():
            try:
                box["value"] = fn(*args)
            except BaseException as e:
                box["error"] = e

        thread = threading.Thread(target=run)
        thread.start()
        thread.join()
        if "error" in box:
            raise box["error"]
        return box["value"]

    def call_with_nested(self, fn):
        """`fn(self.nested)`: a callback given a host object."""
        return fn(self.nested)

    def call_twice(self, fn, x):
        return (fn(x), fn(x))

    def returned_type(self, fn):
        return type(fn()).__name__

    def calling_raised(self, fn):
        """The exception's type name when calling `fn` raises, else None; kept in
        `last_raised` too, for a caller whose own execution the exception ended."""
        try:
            fn()
        except BaseException as e:
            self.last_raised = type(e).__name__
            return self.last_raised
        self.last_raised = None
        return None

    def keep(self, fn):
        self.kept = fn

    def call_kept(self):
        return self.kept(1)

    def probe(self, fn):
        """What a callback allows besides being called, during the call: the exception's type
        for each attempt, or 'ok'."""
        import copy

        results = {}
        for name, attempt in (
            ("attr", lambda: fn.anything),
            ("repr", lambda: repr(fn)),
            ("pickle", lambda: pickle.dumps(fn)),
            ("copy", lambda: copy.copy(fn)),
            ("call", lambda: fn(3)),
        ):
            try:
                results[name] = ("ok", repr(attempt()))
            except Exception as e:
                results[name] = (type(e).__name__, str(e)[:60])
        return results

    def big(self, n):
        return os.urandom(n)

    def gen(self):
        yield 1
        yield 2

    def frames(self):
        return (sys._getframe(),)

    def frames_with_nested(self):
        return (self.nested, sys._getframe())

    def call_with_bytes(self, fn, n):
        """Call `fn` with `n` random bytes; the error's text if that raises, else the result."""
        try:
            return fn(os.urandom(n))
        except Exception as e:
            return f"{type(e).__name__}: {e}"

    def note_raised(self, fn):
        """What calling `fn` raised, with a note added to it here, as `(type name, notes)`."""
        try:
            fn()
        except Exception as e:
            e.add_note("noted on the host")
            return (type(e).__name__, list(e.__notes__))
        return None

    def raise_fixture(self):
        raise FixtureError("fixture failed", 3)

    def raise_command(self):
        raise CommandError(["git", "push"], 128)

    def unpickled(self):
        return unpickled

    def call_after(self, seconds, fn, started=None):
        """Call `fn` after `seconds`, creating `started` first."""
        if started:
            open(started, "w").close()
        time.sleep(seconds)
        return fn()

    def stamp(self):
        """The wall clock as the call runs, in seconds."""
        return time.time()

    def touch(self, path):
        """Create `path`: a call whose effect a test can look for."""
        open(path, "w").close()
        return path

    def marked(self, base):
        return Marked(base)

    def takes_marked(self, obj):
        return type(obj).__name__


def make():
    return Root()


class Vivid(dict):
    """An attribute-dict that makes up what it lacks, as `addict.Dict` does: reading a missing
    attribute stores and returns a new, empty one -- `____id_pack__` among them, which RPyC asks
    an object for, and which is unhashable."""

    def __getattr__(self, name):
        if name.startswith("__") and name.endswith("__") and name != "____id_pack__":
            raise AttributeError(name)
        return self.setdefault(name, Vivid())

    def __setattr__(self, name, value):
        self[name] = value


def make_vivid():
    root = Vivid()
    root.name = "root"
    return root


def make_stubborn():
    """A binding that ignores SIGTERM, with a child that ignores it too: what only SIGKILL ends.
    Set here, on the main thread, where `signal.signal` is allowed."""
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    subprocess.Popen(["sh", "-c", 'trap "" TERM; sleep 1000'])
    return Root()


def make_failing():
    """A factory that starts a program and then raises, writing its process group to
    `OUTRIG_FIXTURE_PGID_FILE` first, so a test can look for what it left behind."""
    subprocess.Popen(["sleep", "1000"])
    with open(os.environ["OUTRIG_FIXTURE_PGID_FILE"], "w") as f:
        f.write(str(os.getpgrp()))
    raise RuntimeError("the factory failed on purpose")


def make_slow():
    """A factory that starts a program, writes its process group to `OUTRIG_FIXTURE_PGID_FILE`,
    and then takes longer than any owner waits."""
    subprocess.Popen(["sleep", "1000"])
    with open(os.environ["OUTRIG_FIXTURE_PGID_FILE"], "w") as f:
        f.write(str(os.getpgrp()))
    time.sleep(1000)
    return Root()
