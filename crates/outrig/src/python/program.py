"""The bootstrap of a host program: what `outrig run <script.py>` gives the payload's interpreter
as its `-c` argument, ahead of the operator's program.

    python3 -I -c <this> <package-dir> <fd-in> <fd-out> <script> [args...]

The interpreter runs isolated, so neither the working directory, `PYTHONPATH` nor the user site is
on `sys.path`. This puts OutRig's package after the standard library, connects `outrig` to the
engine over the two descriptors the engine passed down, puts the script's directory last, and runs
the script as `__main__` with the arguments after it as its `sys.argv`. What the script sees of
how it was started is what `python3 <script> [args...]` would have shown it, and its streams,
working directory, environment and exit status are its own.
"""

import builtins
import io
import os
import sys
import types
from pkgutil import read_code


def _from_the_script(hook, script):
    """`hook`, given an exception whose traceback starts at the script's own frame: this
    bootstrap's are cut, as `python3 <script>` shows none of its own. A syntax error has
    no frame of the script's, and shows none; anything else without one is this bootstrap's, and
    shows whole. Set on the exception, since that is the traceback Python prints."""

    def excepthook(kind, value, tb):
        start = tb
        while start is not None and start.tb_frame.f_code.co_filename != script:
            start = start.tb_next
        if start is None and not issubclass(kind, SyntaxError):
            start = tb
        if value is not None:
            value.__traceback__ = start
        hook(kind, value, start)

    return excepthook


def main():
    if len(sys.argv) < 5:
        os.write(2, b"outrig-program: usage: <package> <fd-in> <fd-out> <script> [args...]\n")
        os._exit(2)
    package, fd_in, fd_out, script, *args = sys.argv[1:]
    # Before the script's directory, so a module beside it named `outrig` is not what loads.
    sys.path.append(package)
    import outrig._host

    outrig._host.install(int(fd_in), int(fd_out))
    sys.path.append(os.path.dirname(os.path.realpath(script)))
    sys.argv = [script, *args]
    sys.excepthook = _from_the_script(sys.excepthook, script)
    _run(script)


def _run(script):
    """Run `script` as `__main__`, in a module that stays `sys.modules["__main__"]` until the
    interpreter exits, set up as `runpy.run_path` sets one up. `run_path` itself puts this
    bootstrap's module back when the script's body returns, before the exit handlers run, so an
    exit handler would find none of the script's names in `__main__`, nor unpickle its classes.

    An exception leaves as it would leave a script: `SystemExit` with its status, anything else
    through `sys.excepthook` with status 1, and `KeyboardInterrupt` by SIGINT."""
    main = types.ModuleType("__main__")
    main.__dict__.update(
        __file__=script,
        __cached__=None,
        __loader__=None,
        __package__=None,
        __spec__=None,
        __builtins__=builtins,
    )
    sys.modules["__main__"] = main
    exec(_compiled(script), main.__dict__)


def _compiled(script):
    """The script's code: compiled from its source, or read from a compiled file, which
    `python3` and `runpy` run too."""
    path = os.path.abspath(script)
    with io.open_code(path) as f:
        code = read_code(f)
    if code is None:
        with io.open_code(path) as f:
            code = compile(f.read(), script, "exec")
    return code


if __name__ == "__main__":
    main()
