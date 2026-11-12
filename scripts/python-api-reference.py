#!/usr/bin/env python3
"""Python API reference rendering.

Renders `doc/reference/python.md` from the docstrings of the `outrig` Python package in
`crates/outrig/src/python/outrig/`: each name a program on the host uses, with its declaration
and its docstring, which is what `help()` shows. The source is read, never imported: the package
needs a live connection to an engine.

  Check -- the default. Renders in memory and diffs against the committed page; writes nothing.
  Write -- `--write`. Writes the page. This is the command to run after a docstring changes.

Either mode fails, naming what is wrong, when a name `outrig/__init__.py` exports is not on the
page, when the page shows a name it no longer exports, when a public method has no docstring,
and when a line of the page is longer than 100 characters.

Exit code 0 on success, 1 on any failure, with the reason on stderr. Standard library only.
"""

import argparse
import ast
import difflib
import sys
import textwrap
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
PACKAGE = REPO_ROOT / "crates" / "outrig" / "src" / "python" / "outrig"
PAGE = "doc/reference/python.md"
MAX_WIDTH = 100
DIFF_MAX = 60  # the diff lines a failed check prints

LEAD = (
    "The names of the `outrig` package a program `outrig run-new <script.py>` runs on the host "
    "uses, each with its declaration and its docstring, which is what `help()` shows; the last "
    "section lists those only agent code uses. [Host programs](../usage/host-programs.md) says how "
    "such a program runs, and [Python Runtime](../concepts/python-runtime.md) describes the agents "
    "it drives. Generated from the package's source by `scripts/python-api-reference.py`: change a "
    "docstring, then run it with `--write`."
)

# Each section: its title, its intro, the file its names are defined in, and the names, each as a
# program writes it -- `outrig.<name>` for one the package exports, the bare name for a class a
# program meets only as a value. A public module's section opens with the module's docstring.
SECTIONS = [
    (
        "`outrig`",
        "The package's own docstring, which `help(outrig)` shows: what each part of the package "
        "is for, and where it runs.",
        "__init__.py",
        [],
    ),
    (
        "Configuration",
        "`outrig.config()` returns the configuration `outrig run-new` loaded, a `Config`, whose "
        "parts are the classes after it. A program meets these classes only as values: none is a "
        "name in `outrig`.",
        "_host.py",
        ["outrig.config", "Config", "Model", "Alias", "Provider", "Events"],
    ),
    (
        "Sessions",
        "A program runs agents in a session it owns: an `outrig.Session`, entered with `async "
        "with`, which `outrig.current_session()` returns inside the block. An `outrig.Mount` is a "
        "directory its container sees, an `outrig.Application` the code its agent classes are "
        "in, and its `events()` yields an `outrig.Event` for each record, with an "
        "`outrig.Missed` where records were lost. See [Host "
        "programs](../usage/host-programs.md#sessions).",
        "_host.py",
        [
            "outrig.Session",
            "outrig.Mount",
            "outrig.Application",
            "outrig.current_session",
            "outrig.Event",
            "outrig.Missed",
        ],
    ),
    (
        "Shutdown",
        "Leaving a session's `async with` block shuts it down and sets its `shutdown_report`, an "
        "`outrig.ShutdownReport`, whose fields hold the classes after it, met only as values. "
        "When the engine cannot prove that everything the session started stopped, leaving raises "
        "`outrig.ShutdownException`.",
        "_host.py",
        [
            "outrig.ShutdownReport",
            "ClosedBy",
            "Stopped",
            "ExecutionOutcome",
            "HostedCall",
            "ServiceCall",
            "RequestOutcome",
            "BindingStop",
            "EventDelivery",
            "LogLoss",
            "outrig.ShutdownException",
        ],
    ),
    (
        "Agents",
        "A program's agents are instances of `outrig.Agent` subclasses, each a child agent in a "
        "session the program owns, whose methods marked `@outrig.agent` are its request channels. "
        "A call of one returns a `Handle` at once, whose `status` is an `outrig.Status`; a program "
        "never constructs a `Handle`. A class adds to its child's instructions by returning "
        "`outrig.Section`s from `__outrig_instructions__`. See "
        "[Host programs](../usage/host-programs.md#agent-classes).",
        "_agents.py",
        ["outrig.Agent", "outrig.agent", "Handle", "outrig.Status", "outrig.Section"],
    ),
    (
        "Errors",
        "A request that ends without an accepted answer raises one of these from `await handle`, "
        "unless it failed on the caller's side: a typed call whose `validate=` raised raises that "
        "exception, and a model that is not configured, or work too long to send, raises "
        "`ValueError`. Each of these is an `outrig.AgentError`, so `except outrig.AgentError` "
        "catches them all and lets the caller's errors through.",
        "_agents.py",
        [
            "outrig.AgentError",
            "outrig.CompletionRejected",
            "outrig.AgentRequestFailed",
            "outrig.RequestCancelled",
            "outrig.AgentReleased",
            "outrig.AgentLimitReached",
            "outrig.BudgetExhausted",
            "outrig.SessionClosing",
            "outrig.RequestSettled",
        ],
    ),
    (
        "`outrig.schema`",
        "The types an agent class's requests and replies carry are ones `outrig.schema` takes. Its "
        "docstring, which `help(outrig.schema)` shows, says which.",
        "schema.py",
        [
            "outrig.schema.declare",
            "outrig.schema.Schema",
            "outrig.schema.doc",
            "outrig.schema.alias",
            "outrig.schema.DecodeError",
            "outrig.schema.Problem",
            "outrig.schema.PROBLEMS_MAX",
        ],
    ),
]

KERNEL_INTRO = (
    "Agent code in an agent's kernel uses these names, as [Python "
    "Runtime](../concepts/python-runtime.md) describes; a program on the host has no use for them. "
    "What a program meets if it reads one:"
)
# The names only an agent's kernel uses, with what a program on the host meets if it reads one.
KERNEL_ONLY = {
    "runtime": "reading it raises `RuntimeError`.",
    "bindings": "reading it raises `RuntimeError`.",
    "skills": "each of its functions raises `RuntimeError`.",
    "Request": "declares a channel for `runtime.spawn`, which a program does not have.",
    "facility": "makes a reference, which `facilities=` refuses on the host.",
    "Facility": "what `outrig.facility` makes; each use of one raises `RuntimeError`.",
    "Service": "the class of a service's object, which only a kernel holds.",
    "ServiceError": "what a call of a service's method raises, which only a kernel makes.",
    "ServiceMethod": "what a service's object lists of its methods, which only a kernel holds.",
}

# Never constructed by a program; their constructors take internals.
HIDDEN_CONSTRUCTORS = {"Handle", "Schema"}

FUNCTIONS = (ast.FunctionDef, ast.AsyncFunctionDef)


def fail(message):
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def fill(text):
    return textwrap.fill(text, MAX_WIDTH, break_long_words=False, break_on_hyphens=False)


def parse(name):
    return ast.parse((PACKAGE / name).read_text(encoding="utf-8"))


def defined(tree):
    """Each def, class and assignment at `tree`'s top level, by the name it binds."""
    return {
        getattr(node, "name", None) or ast.unparse(node.targets[0]): node
        for node in tree.body
        if isinstance(node, (ast.ClassDef, ast.Assign, *FUNCTIONS))
    }


def declaration(node):
    """`node` as a stub declares it: a function's decorators and `def` line; a class's decorators,
    `class` line, fields and constructor; or an assignment."""
    if isinstance(node, (ast.Assign, ast.AnnAssign)):
        return ast.unparse(node)
    lines = [f"@{ast.unparse(d)}" for d in node.decorator_list]
    if isinstance(node, FUNCTIONS):
        kind = "async def" if isinstance(node, ast.AsyncFunctionDef) else "def"
        returns = f" -> {ast.unparse(node.returns)}" if node.returns else ""
        return "\n".join(lines + [f"{kind} {node.name}({ast.unparse(node.args)}){returns}: ..."])
    body = [
        textwrap.indent(declaration(m), "    ")
        for m in node.body
        if isinstance(m, ast.AnnAssign)
        or getattr(m, "name", "") == "__init__" and node.name not in HIDDEN_CONSTRUCTORS
    ]
    bases = f"({', '.join(map(ast.unparse, node.bases))})" if node.bases else ""
    return "\n".join(lines + [f"class {node.name}{bases}:" + ("" if body else " ...")] + body)


def entry(title, node, level=3):
    """The blocks documenting `node` as `title`: its heading, declaration and docstring, then each
    public method's and property's, a heading level down."""
    blocks = [f"{'#' * level} `{title}`", f"```python\n{declaration(node)}\n```"]
    if isinstance(node, ast.Assign):  # a constant, which has no docstring
        return blocks
    blocks.append(ast.get_docstring(node) or fail(f"{title} has no docstring"))
    for member in node.body if isinstance(node, ast.ClassDef) else []:
        if isinstance(member, FUNCTIONS) and not member.name.startswith("_"):
            blocks += entry(f"{title}.{member.name}", member, level + 1)
    return blocks


def render():
    """The page, failing on a name it would leave out or show wrongly, or on a line too long."""
    blocks = ["# Python API Reference", fill(LEAD)]
    for title, intro, file, names in SECTIONS:
        tree = parse(file)
        blocks += [f"## {title}", fill(intro)]
        if file in ("__init__.py", "schema.py"):
            blocks.append(ast.get_docstring(tree) or fail(f"{file} has no docstring"))
        defs = defined(tree)
        for name in names:
            bare = name.rpartition(".")[2]
            blocks += entry(name, defs.get(bare) or fail(f"{file} defines no {bare}"))
    blocks += ["## Kernel-only names", fill(KERNEL_INTRO)]
    blocks.append("\n".join(f"- `outrig.{name}` -- {what}" for name, what in KERNEL_ONLY.items()))
    page = "\n\n".join(blocks) + "\n"

    init = defined(parse("__init__.py"))
    exported = {
        const.value
        for name in ("__all__", "_HOST_ONLY", "_KERNEL_ONLY")
        for const in ast.walk(init.get(name) or fail(f"outrig/__init__.py assigns no {name}"))
        if isinstance(const, ast.Constant) and isinstance(const.value, str)
    }
    shown = {n.split(".")[1] for *_, names in SECTIONS for n in names if n.startswith("outrig.")}
    shown |= set(KERNEL_ONLY)
    if exported != shown:
        fail(
            "the page and what outrig/__init__.py exports disagree"
            + "".join(f"\n  not on the page: outrig.{n}" for n in sorted(exported - shown))
            + "".join(f"\n  no longer exported: outrig.{n}" for n in sorted(shown - exported))
        )

    long = [
        f"\n  line {number}, {len(line)} characters: {line}"
        for number, line in enumerate(page.splitlines(), 1)
        if len(line) > MAX_WIDTH
    ]
    if long:
        fail(f"{PAGE} would have lines longer than {MAX_WIDTH} characters" + "".join(long))
    return page


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--write", action="store_true", help="write the page instead of diffing it")
    args = parser.parse_args()

    page = render()
    path = REPO_ROOT / PAGE
    if args.write:
        path.write_text(page, encoding="utf-8")
        print(f"{PAGE}: written")
        return 0

    committed = path.read_text(encoding="utf-8") if path.exists() else ""
    diff = list(
        difflib.unified_diff(
            committed.splitlines(),
            page.splitlines(),
            f"{PAGE} (committed)",
            f"{PAGE} (generated)",
            lineterm="",
        )
    )
    if not diff:
        print(f"{PAGE}: OK")
        return 0
    more = f"\n... {len(diff) - DIFF_MAX} more diff lines" if len(diff) > DIFF_MAX else ""
    fail(
        f"{PAGE} is not what the docstrings render. Regenerate it with\n"
        "    python3 scripts/python-api-reference.py --write\n" + "\n".join(diff[:DIFF_MAX]) + more
    )


if __name__ == "__main__":
    sys.exit(main())
