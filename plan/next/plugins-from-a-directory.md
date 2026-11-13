# A plugin comes from a directory, or from an embedder

## Context

#933 makes a plugin a named recipe OutRig ships: a library, the glue that builds its object on
the host, and the description the agent reads, turned on with `[plugins.<name>]` and lowered to
an ordinary binding (#934). The set is fixed by the release, so adding a plugin means changing
OutRig. Someone with a library of their own still writes a `[bindings.<name>]` entry by hand, and
has nowhere to put glue: a factory must import from a `requires` package, and `requires` refuses
paths.

## Shape

- **A directory.** A plugin as a folder with a small metadata file and a Python module holding
  the factory, found in the user's home directory and in the repository, as a skill is found
  (`plan/phase/0003-python/skills.md`, "Layout"). Unlike a skill, its code runs on the host as
  the user, so:
  - A repository's plugin needs the approval a repository's binding needs, by a digest that
    covers the module's bytes and not only the declaration.
  - The module is read from the host, never through a path the container can write between the
    approval and the start.
  - Whether installing one is what turns it on, or a `[plugins.<name>]` line still does, is
    open. The user's answer when #933 was filed was that both are good options.
- **An embedder.** A program using the `outrig` library registers plugins of its own, by name,
  beside the ones OutRig ships. `SessionBuilder::bind` already takes a `BindingSpec`; what is
  missing is glue the embedder supplies, which a Rust program has no Python package for.
- **Shadowing.** A directory plugin with the name of one OutRig ships: refuse, or report the
  shadowed one as skills do.

## Acceptance

- A plugin directory in the user's home gives the agent its object with one line of config or
  none, whichever the design picks, and no `[bindings.<name>]` entry.
- A repository's plugin does not start until approved, and an edit to its module asks again.
- An embedder's plugin is turned on by the name the embedder gave it.
