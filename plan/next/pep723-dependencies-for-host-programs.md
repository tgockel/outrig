# A host program's PEP 723 dependencies are not installed

## Context

`outrig run-new <script.py>` (`0003-27`) runs a program with the standard library, the `outrig`
package and the modules beside the script; nothing installs a dependency the script declares in a
`# /// script` block. The host-program requirements list installation as optional (R8): the
prototype needs only the standard library.

## Shape

- Off by default, as skill dependencies are (`plan/next/skill-dependency-installation.md`).
- Pure-Python wheels only, through the check `0003-18` applies to a binding's install, into a
  cache keyed by the requirement set and put on the program's `sys.path` before it runs; a
  requirement with no pure-Python wheel fails the launch with the reason and installs nothing.
- The block is read lexically, as `0003-32` reads a skill's, before any of the program runs.
- An installer, a lock rule and an index policy are chosen then, not here.

## Acceptance

- A program declaring a missing pure-Python package imports it once installation is on; a second
  launch installs nothing; a compiled requirement fails the launch with the reason.
