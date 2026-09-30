# The payload's pip shares its user site with an image Python of the same version

## Context

`0003-10` made plain `pip install` from the agent's code work. The payload is mounted read-only, so
its pip falls back to a user install into `~/.local/lib/python3.13/site-packages`, and the
interpreter reads that directory. That path is not the payload's alone. It is the user site of
any Python 3.13 in the image -- a `python:3.13` image, Debian trixie, Fedora 41 and later -- and
the sharing runs both ways:

- The agent's `pip install requests==X` lands in the image Python's user site, which comes ahead
  of that Python's own site-packages. So what the project's `python3 -m pytest` imports changes,
  silently, as a side effect of the agent installing something for its own interpreter. A project
  that runs from a virtual environment is unaffected, since a venv ignores the user site.
- The route `runtime.python` recommends for compiled packages -- run the image's Python, where
  `python3 -m pip install numpy` installs for it -- falls back to the same user site when the
  image's site-packages is not writable, as it is not for a non-root user. numpy's glibc build then
  sits on the agent interpreter's `sys.path`, where `0003-10`'s diagnostic explains the failure
  rather than preventing it.

Found by `/simplify`'s altitude review of `0003-10`; left out of that task because the fix
reaches past it.

## Shape

- Give the payload a site that no other Python reads. Python's own mechanism is a virtual
  environment laid over the payload, made under `HOME` when the interpreter starts: a `pyvenv.cfg`
  with `home = /outrig/python/bin` and `include-system-site-packages = true`, a `bin/python3.13`
  link, and an empty `site-packages`.
  - The `pip` on `PATH` runs `<venv>/bin/python3.13 -m pip`, so it installs into the venv
    directly, with no fallback and no `--user`. It has to be a small script rather than a link,
    since the payload's own `pip` script resolves its interpreter through `realpath`.
  - `_open_imports` appends the venv's site-packages instead of the user site.
  - `sys.executable -m pip`, which a model also writes, reaches the payload interpreter, not the
    venv, and would still fall back to the user site. Either point `sys.executable` at the venv's
    interpreter -- which `multiprocessing` also uses to spawn -- or keep the user site on the path
    as well and document it.
  - Console scripts pip installs get the venv's interpreter in their shebang, so they find their
    packages without `PYTHONUSERBASE` exported to every child.
- Setting `PYTHONUSERBASE` for the payload's pip alone does not work. The scripts pip installs
  would need it too, so it would have to reach every process the interpreter starts, the image's
  Python included.

## Acceptance

- In an image with Python 3.13, a plain `pip install` from the agent's code imports in the agent's
  interpreter and is not importable by the image's `python3`.
- `python3 -m pip install --user` run for the image's Python does not put anything on the agent
  interpreter's `sys.path`.
- The `0003-10` pip tests pass unchanged, and the host-side one no longer needs `--user`.
