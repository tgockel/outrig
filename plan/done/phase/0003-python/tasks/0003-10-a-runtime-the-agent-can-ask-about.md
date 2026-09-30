# 0003-10 -- The agent can ask what it holds and what it cannot import

## Context

A persistent namespace only helps if the agent can find out what is in it. `discovery.md` splits
that into three questions -- what do I have, what is this thing, what can this interpreter do --
and settles the constraint that shapes all three: **automatic observation must never execute code
the agent wrote.**

That constraint has a boundary rather than being absolute, and the boundary is worth implementing
deliberately. The echo on a successful execution calls `repr()`, which *is* agent code. It runs
inside the agent's own execution, so a looping `__repr__` wedges that execution and the interrupt
path already applies. The inventory runs outside any execution, at the host's request, and must
therefore stay to names and types.

The third question has the worst failure mode. The interpreter cannot load third-party extension
modules -- not because they are unavailable but because there is no mechanism -- and an agent must
not learn this by repeated failed `pip install` attempts, which look to a model like a permissions
problem it might route around.

## Goal

An agent can discover an unfamiliar method rather than guess it, list what it holds, and get a
straight answer about what this interpreter cannot run.

## Deliverables

- The bounded inventory, already in the protocol, plus **a way to find what it omitted**: a count
  and a means of asking for the rest, since a cap of two hundred names silently becomes a lie at
  two hundred and one.
- Ordinary Python as the answer to "what is this thing": `help()`, `inspect`, `__doc__`. Bounded
  output, and asking is an explicit act rather than a preamble entry.
- An import and capability manifest, so "can I use `numpy`" has an answer before the attempt.
- **A diagnostic on a failed import of a compiled module** that names the reason and the route --
  an image-provided Python, a subprocess, a service -- rather than surfacing a bare
  `ModuleNotFoundError`.
- **Plain `pip install`, run from the agent's code, works for a pure-Python package**, and the
  package imports in the running interpreter without a restart. The payload already carries
  pip, and OutRig's security model allows the agent to install; what stopped it was where pip
  put the package and where the interpreter looked. `pip install --target DIR` with `DIR`
  added to `sys.path` keeps working too. A package with compiled parts still cannot load, and
  the route for it -- running the agent's interpreter on the image's own Python -- is filed in
  `plan/next/` rather than built here.
- The preamble, per the rule: what an agent cannot learn by looking *and* needs every round.
  Names persist across rounds; observations are bounded while the values behind them are not;
  waiting watches channels; `pip install` adds pure-Python packages, and nothing compiled loads.
- **The echo/inventory boundary implemented as designed**: the echo renders inside the execution,
  the inventory never calls `repr`, a property, a descriptor, or an iterator.

## Acceptance

- A value with a looping `__repr__` as the trailing expression wedges only that execution, and the
  interrupt path recovers it. The inventory taken afterwards still answers.
- **An inventory of an over-cap namespace reports the count and offers the rest**, rather than
  silently truncating.
- `help()` on a runtime object returns a signature and docstring, bounded.
- Importing a compiled third-party module produces the actionable diagnostic, not a bare
  `ModuleNotFoundError`. A pure-Python import from the workspace succeeds -- worth testing
  explicitly, because it is the case an agent reaches for first.
- In a real container, with the payload mounted read-only, a plain `pip install` of a
  pure-Python package from the agent's code is followed by a successful import in the same
  interpreter.
- Processing a value larger than the context window works: the value stays whole in Python and
  only the observation is clipped.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether proxies need their own description path -- defer.** A Pyro proxy's signature is not
   locally knowable, so `help()` on one either lies or round-trips. Nothing exposes proxies until
   the credential work, which is unqueued.
2. **Preamble versus module docstring for the runtime guide -- both, split by the rule.** The
   preamble keeps what is needed every round and one sentence naming `help(runtime)`; the
   manifest and the rest of the runtime's surface are docstrings read on demand.

## Dependencies

- **Hard: 0003-05.** There is no preamble to put anything in until the command exists.
- **Hard: 0003-06.** The looping-`__repr__` criterion below asserts the interrupt path recovers
  the execution, which does not exist until then -- numeric order places this task later anyway,
  but the dependency is the reason rather than a coincidence.
- **Soft: 0003-09**, whose channel-watching sentence is one of the preamble's entries.

## See also

- `plan/phase/0003-python/discovery.md` -- the three questions, the safe-observation constraint,
  and the preamble rule.
- `plan/phase/0003-python/mcp-wrappers.md` -- what the extension-module limit already cost, which
  is the concrete example the diagnostic should not make an agent rediscover.

## Decisions

- **Plain `pip install` works (the maintainer's call).** Planning found that the preamble's "`pip
  install` does not work" was not quite true, and the maintainer settled what should be:
  installing is allowed by OutRig's security model, so a plain `pip install` from the agent's code
  must work, `--target` must not be prevented, and compiled packages get a `plan/next/` entry for
  running on the image's own Python rather than a workaround here.
  - The payload already carries pip. In a session its site-packages is read-only, so pip installs
    into the user site on its own ("Defaulting to user installation"). The interpreter runs with
    `-I`, which ignores the user site, so `_open_imports` appends it to `sys.path` at start.
  - The directory is made at start. An import that scans a path with no directory there caches
    that, and would not see a package pip later put there. With the directory in place,
    `FileFinder` notices it changed, and a module pip adds mid-session imports without
    `importlib.invalidate_caches()`, which was checked directly.
  - Its `.pth` files are not processed, so nothing installed runs when the interpreter starts. An
    editable install or a namespace package that relies on a `.pth` file does not work as a
    result.
  - `pip` on `PATH` is `~/.local/share/outrig/bin`, holding only `pip`, `pip3`, and `pip3.13`.
    It goes first, then `~/.local/bin`, so console scripts pip installs can run.
    - Each is a short `/bin/sh` script, `_PIP`. It starts the payload's Python isolated, which
      execs `python -m pip` with every `PYTHON*` variable removed except `PYTHONUSERBASE`.
      - An image that sets `PYTHONHOME` for its own Python made the payload's pip exit before
        it initialized, which was measured, since the payload's own script starts Python without
        `-I`.
      - `PYTHONNOUSERSITE` would stop pip's fallback to the user site.
      - `PYTHONUSERBASE` is kept because it says where pip installs, and `site` honors it even
        under `-I`, so the interpreter reads the same place.
      - The variables are cleared for pip and what pip starts in turn, such as a build, and for
        nothing else. The image's Python still gets them.
      - `sys.executable -m pip`, run from agent code, still starts the payload's Python in the
        image's variables.
    - The inner launch runs `python -P -m pip`.
      - `-m` alone would put the working directory, which is the workspace, first on
        `sys.path`, so a project's `pip.py` would run in pip's place and its `types.py` would
        stop pip starting. Both were measured.
      - Not `-I`, which would also turn off the user site pip falls back to.
      - The payload's own script never had the problem, since it runs from `bin/`. An external
        review of the wrapper found it, and the host pip test now runs in a workspace holding
        both files.
    - The first cut symlinked the payload's own scripts, which find their interpreter through
      `realpath "$0"`. An external review found the `PYTHONHOME` failure.
    - Each script is written under a temporary name and renamed over the old one at every start,
      so interpreters starting together under one `HOME` never find one missing or half
      written.
    - The first cut used `tempfile.mkdtemp`, which left a directory in `/tmp` per start and
      nothing to remove it. `/simplify` found over a hundred from one day's test runs.
    - The links are not under `~/.local/bin`, so a `pip install --upgrade pip`, whose script
      lands there, cannot collide with them.
    - The payload's own `bin/` was not put on `PATH`, because it also holds `python3`, which
      would shadow the image's Python.
  - The e2e test in `host_tests.rs` shows the fallback in alpine with the payload mounted
    read-only. The contingency, `PIP_USER=1`, was not needed.
  - On the host the payload is writable, so the host-side test passes `--user` explicitly and
    never writes into the shared cache.
  - Fetching from an index needs network and CA certificates. The interpreter's `ssl` looks for
    them in `/etc/ssl/cert.pem` and `/etc/ssl/certs`. Installing from a real index inside a
    container was not exercised here; only a local wheel was, with `--no-index`.

- **The workspace goes on `sys.path` after the standard library, by absolute path.** It is the
  starting working directory, which the host makes the workspace with `-w`.
  - Appended rather than prepended as Python's REPL does. A project file named like a standard
    module (`types.py`, `colorsys.py`) would otherwise replace it for every later import, the
    interpreter program's own included. A workspace module with a standard module's name
    therefore cannot be imported by that name.
  - An absolute path rather than `''`, so an `os.chdir` does not change what imports.
  - The order is the standard library, the payload's site-packages (pip only), the workspace,
    then the user site. So a workspace module shadows a package of the same name that pip
    installed, as the working directory does in ordinary Python.

- **Failed imports are explained where they are reported, not in the import system.** Both
  places a failure reaches the model call `_explain_imports`: `_format_error`, for an execution's
  own error, and `_attributed_exception`, for a task nobody awaited or a failing callback. It walks
  the exception, its causes and contexts, and an exception group's members, and rewrites the
  message of each failure the interpreter itself is the reason for:
  - a compiled file that was found and cannot load (`Dynamic loading not supported`, with an
    extension suffix on `e.path`);
  - a compiled file built for another interpreter, found where the import looked (a
    `-linux-gnu.so` beside a `ModuleNotFoundError`);
  - a top-level, non-standard module found nowhere.

  The rewrite is one line, key fact first, so the status line's 300-byte cut keeps what matters,
  and it names the image's own Python when one is on `PATH`.
  - An import hook -- `builtins.__import__`, or a finder at the end of `sys.meta_path` -- would
    carry the explanation into code that catches the error. But a finder that raises breaks
    `importlib.util.find_spec` for every library probing an optional import, and a wrapped
    `__import__` puts one of this program's frames into every traceback through an import, and
    does not reach `importlib.import_module`.
  - The cost is that an agent printing a caught `ImportError` itself sees Python's message. A
    test pins that.
  - Only `ImportError` and `ModuleNotFoundError` themselves are read, never a subclass. Causes
    and group members are read through `BaseException`'s own descriptors. So formatting runs none
    of the agent's code, which `_format_error` already guarded against.
  - A package's `__path__` is read only from a plain module, and only if it is a list or
    importlib's `_NamespacePath`. A namespace path is read as it last computed it, from its
    `_path`.
    - Iterating one computes it again when its parent's path has changed, through every path
      hook and finder the agent installed. An import that fails inside a `try` whose `finally`
      restores `sys.path` would have had them run while its failure was reported, outside the
      execution's interrupt boundary. An external review found it.
    - A test installs a finder that records being called, and fails if it is.
  - Where explaining fails, the diagnostic names the failure by its type alone. Its repr is the
    agent's code, and could raise out of the report.

- **`help()` is replaced, and bounded at 8 KiB.** `_Help` calls
  `pydoc.Helper(input=StringIO(), output=StringIO())`, so strings, keywords, and topics behave as
  in pydoc, then cuts at `HELP_MAX`. The marker says how long the whole answer was and to ask
  about one member instead.
  - The cut falls at a line break in the bound's second half when there is one, and mid-line
    otherwise, so a single long line, such as one module's summary, is cut rather than dropped
    whole.
  - The `Helper` is pydoc's with its module search, `help('modules <key>')`, redone to write its
    matches to the answer. pydoc's `apropos` prints them itself, past the bound and ahead of its
    own heading, and redirecting `sys.stdout` around it would take other executions' output
    along. An external review found it.
  - 8 KiB leaves half of `OUTPUT_MAX` for whatever else the execution prints, and holds a function
    or a small class whole. `help(runtime)` and `help(runtime.python)` both fit.
  - A bare `help()` prints a short guide pointing at `help(runtime)` and `help(runtime.python)`.
    `site`'s would start the interactive utility, read end-of-file from `/dev/null`, and print a
    banner describing a prompt that does not exist here.
  - `help('modules')` is not mentioned in the guide. It walks every package on `sys.path`,
    importing them, and the workspace is on it.

- **The inventory reports `total` and `more`, and takes `after`.** A page is the first 200 names
  sorted after `after`. `more` is how many follow the page, and `total` counts every name held,
  so a listing that stops short says so. The next page starts after the last name listed, which
  stays stable when names are bound between pages, where an offset would not.
  - `Interpreter::inventory` still asks for the first page only, since the liveness probe is its
    one caller. No Rust paging API is added until something needs the rest; the protocol is
    tested directly.
  - A name longer than `REPR_MAX` is listed cut short, so a cursor made from it would list that
    name again on the next page. Accepted, since such names do not occur in practice.

- **`runtime.names()` is the agent's inventory, uncapped.** It returns every name as
  `{name: type name}`, from the same `Kernel.held` the protocol inventory uses, so it reads
  nothing but keys and types. It is a value rather than an observation, so it stays whole, and
  showing it is bounded like any other value. The echo cuts it at `REPR_MAX`, and `print` at
  `OUTPUT_MAX`.

- **The manifest is `runtime.python`, with no per-module check** (recommended in planning, and
  not overruled). Its repr is one paragraph:
  - the standard library imports;
  - `pip install` adds pure-Python packages, and the workspace is on `sys.path`;
  - nothing compiled loads;
  - the image's own Python on `PATH` is the route.

  Its docstring is the full manifest. `version`, `workspace`, and `image_python` are properties,
  `image_python` looked up when read. An `importable(name)` was considered and left out. Without
  importing, it cannot know whether a package's compiled parts are optional accelerators, nor
  whether a pure-Python package's own dependencies are there. `importlib.util.find_spec` already
  answers whether a module is present at all, and the docstring says so.

- **The runtime guide is split by the preamble rule** (fork 2; recommended in planning, and not
  overruled).
  - The orientation's last paragraph now says pip adds pure-Python packages, nothing compiled
    loads, `runtime.python` has the details, and `help(runtime)` describes the rest.
  - "Observations are bounded while the values behind them are not" went into `submit_python`'s
    description, which already carried that output is bounded. The preamble does not repeat it.
  - `help(runtime)` lists `runtime.names()` and `runtime.python` in `Runtime`'s docstring.

- **Tests never touch the developer's home.** Every interpreter started on the host now makes the
  user site under `HOME`, so both harnesses set `HOME` and remove `PYTHONUSERBASE`, which `site`
  honors even under `-I`:
  - `testing::spawn` uses `testing::host_home()`, a per-user directory under the system temp
    directory;
  - `interpreter_tests` uses the same by default, and a test can give its own through `Start`;
  - the tests' pip runs with `--isolated`, so a developer's `PIP_*` variables and `pip.conf`
    cannot fail them.

- **The workspace sentence stays conditional.** "Its Python modules import too" joined the
  orientation's workspace line rather than the capability paragraph, so an agent with no known
  workspace is told nothing about one, as before.

- **After `/simplify`:**
  - `Inventory` derives `Deserialize` and `Reply::Inv` flattens it, so a new field is one edit.
  - `Fake::inventory` is the one empty reply the test fakes send.
  - `_compiled_file` compares a namespace path's type by identity with importlib's
    `_NamespacePath`. The first cut compared its `__name__`, which an agent's class could
    match or a metaclass could compute.
  - `Python.__repr__` ends with `_elsewhere()`, the same sentence the diagnostic uses, and the
    `ready` greeting and `runtime.python.version` share `_VERSION`.
  - Left as they are:
    - `after`, which the deliverable asks for, though only tests send it.
    - The duplicate over-cap setup in the older inventory test.
    - The separate e2e container for the pip check, kept apart for clarity.
    - `Kernel.held` naming every binding's type before the cut, measured at under 2 ms for
      10,000 names.
    - A failed inventory, which still answers empty with `total` and `more` of zero. That was
      the behavior before, and the probe only needs an answer it can parse.

- **Follow-ups filed:**
  - `plan/next/the-payloads-pip-shares-the-user-site.md`: the user site pip installs into is
    also the user site of any Python 3.13 in the image, both ways. A venv laid over the payload
    would make it private. Found by `/simplify`, and left out as reaching past this task.
  - `plan/next/use-the-images-python.md` is the numpy route the maintainer asked for.
  - `plan/next/pip-builds-what-it-cannot-load.md`: `pip debug --verbose` on the payload lists
    only `linux_x86_64` tags, so every compiled package goes to a source build that is slow to
    fail, or with a compiler present, installs a module that cannot load.
