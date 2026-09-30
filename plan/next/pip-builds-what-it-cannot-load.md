# pip builds compiled packages the interpreter can never load

## Context

`0003-10` made plain `pip install` work from the agent's code: the payload's own `pip` is first on
`PATH`, it installs into the user site, and the running interpreter imports the package at once.
That is right for pure-Python packages and wasted work for everything else.

The payload's interpreter is a static binary. `packaging`, which pip uses to decide which wheels
fit, finds no glibc version and no musl loader in it, so the only platform tag it offers is plain
`linux_x86_64` (or `linux_aarch64`) -- `pip debug --verbose` on the payload lists no `manylinux`
or `musllinux` tag at all. PyPI does not host wheels with that tag, so every package with
compiled parts goes to a source build. Measured on the host with `pip install numpy --target DIR`:
pip fetched numpy's source and failed while installing its build backend's own compiled
dependencies. Two outcomes are possible, and neither is useful:

- Without a compiler in the image, the build fails after downloading and trying, with an error
  about build dependencies that says nothing about the real reason.
- With a compiler, the build may succeed and install a `.so` file the interpreter then refuses:
  `Dynamic loading not supported`, explained by `0003-10`'s import diagnostic, but only after a
  long build.

Either way the model spends a round, and possibly minutes, learning what the manifest already told
it.

## Shape

- Make the payload's pip refuse a distribution that is not pure Python, up front, with a message
  that says why and names the image's own Python as the route. Two ways to get there, to be
  weighed:
  - Configure pip for this interpreter only, through the `pip` link `_open_imports` puts on
    `PATH` (a wrapper setting `PIP_ONLY_BINARY=:all:` would also refuse pure-Python packages that
    ship only an sdist, so that alone is too blunt).
  - Let the build happen but refuse to install a wheel that is not `py3-none-any` or
    `*-none-any`, which needs a hook pip does not offer, so probably a wrapper around `pip
    install`.
- Whatever is chosen must not affect the image's own pip, reached as `python3 -m pip`.

## Acceptance

- In a session container, `pip install` of a package that has no pure-Python wheel ends quickly
  with a message naming the reason, and installs nothing.
- `pip install` of a pure-Python package that ships only an sdist still installs and imports.
