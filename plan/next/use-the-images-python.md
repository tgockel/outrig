# Let a session run the agent's interpreter on the image's own Python

## Context

The agent's interpreter is OutRig's static CPython, mounted read-only at `/outrig/python`, which
is what lets `run-new` work in an image with no Python at all. The same build cannot load
compiled code: there is no dynamic loader in a static binary, so a package with compiled parts --
numpy, pandas, pydantic, lxml, cryptography -- does not import however it is installed. `0003-10`
made plain `pip install` work for pure-Python packages and made the failure for a compiled one say
why, but the wall itself stays.

Today the agent's route around it is a subprocess: run code that needs numpy with the image's
`python3`, and read back what it prints. That works for a one-off computation and is poor for the
thing this phase is built around, which is keeping data in the interpreter's namespace across
rounds. A dataframe cannot live in a subprocess that has exited.

An image that already carries a Python with the packages its project needs -- a data-science
image, or the project's own dev image -- could run the interpreter program on that Python instead,
and the agent would import what the project imports.

## Shape

- A config choice, per image or per agent, naming the interpreter to run: OutRig's (the default)
  or the image's, by path or as `python3` on the image's `PATH`. The host launches
  `<that python> -I -c PROGRAM <agent-id>` exactly as it launches the payload's today
  (`Interpreter::start` in `crates/outrig/src/python/host.rs`).
- A startup check that the image's Python can run the program, with an error that says what is
  missing. The program needs at least:
  - Python 3.13, or whatever version the program is written for. It uses `asyncio` internals,
    `sys.stdlib_module_names`, `BaseExceptionGroup`, and 3.13's traceback formatting. Either pin
    a minimum version or test a range.
  - The standard modules it imports at start, `resource`, `mmap`, and `signal` among them.
  - A working `/dev/null`, and `pthread_kill` on the main thread for interrupts.
- What changes, and has to be stated where the option is documented:
  - The memory ceiling still applies (`RLIMIT_DATA`), but compiled code allocates outside
    Python's view. A native extension that ignores `MemoryError`, or aborts on a failed `malloc`,
    ends the interpreter, and every agent in it, rather than reporting.
  - Native code holding the GIL cannot be interrupted. Recovery already has a verdict for this
    (`Verdict::Starved`), but with numpy it becomes an ordinary event rather than a pathological
    one.
  - `pip` should then mean the image's Python's pip, installing where that Python looks. The
    `PATH` shim `_open_imports` sets up is the payload's and would have to follow the choice.
  - The image's Python may be externally managed (PEP 668), which refuses a plain
    `pip install`. Say what happens then rather than working around it.
- What is lost: working in an image without Python, and one interpreter version to test against.
  That is why this is an option and not the default.

## Acceptance

- With the option set, in an image that has a Python with numpy, `import numpy` in the agent's
  code succeeds, and an array bound in one execution is read by the next.
- The interrupt, runaway, memory-ceiling, and user-channel suites pass against that Python as
  they do against the payload, or the ones that cannot are named in the documentation.
- An image whose Python is too old, or missing a module the program needs, fails at startup with
  a message naming the version or the module, not with a traceback from the program.
- Without the option, nothing changes.
