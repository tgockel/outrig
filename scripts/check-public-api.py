#!/usr/bin/env python3
"""Public-API snapshot gating.

Regenerates the `crates/*/public-api.txt` surface snapshots on the toolchain pinned in
`[workspace.metadata.public-api]` (root `Cargo.toml`) and diffs the result against what is
committed, so a surface change that forgets to regenerate fails here instead of rotting.

  Check     -- the default. Generates in memory and diffs; writes no tracked file.
  Write     -- `--write`. Regenerates the snapshots in place. This is the command to run
               after an intentional surface change.
  Self-test -- `--self-test`. Checks that the comparison rejects both a removed and an added
               item and names it. Needs neither the toolchain nor the tool.

Exit code 0 on success, 1 when a surface differs, 2 when the environment or the pinned tooling
is wrong. That split is the point: a rustdoc JSON format mismatch is not a surface change, and
a red run has to say which of the two it is before anyone starts reading a diff.

Standard library only; no `pip install` step needed in CI.
"""

from __future__ import annotations

import argparse
import difflib
import os
import platform
import subprocess
import sys
import traceback
from pathlib import Path

if sys.version_info < (3, 11):
    # Checked before the import rather than after, so a too-old interpreter is reported as the
    # tooling fault it is. A bare `ImportError` would exit 1, which this script reserves for
    # "the surface differs" -- the one thing a missing parser cannot possibly have observed.
    print(
        "error: this needs Python 3.11 or newer for `tomllib`; this is "
        f"{sys.version_info.major}.{sys.version_info.minor}",
        file=sys.stderr,
    )
    raise SystemExit(2)

import tomllib  # noqa: E402  -- deliberately after the version guard above

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "Cargo.toml"

# Generated artifacts go here rather than into `target/debug`: that tree belongs to the stable
# toolchain the rest of CI uses, and interleaving two toolchains in one target directory buys a
# full rebuild in both directions.
TARGET_DIR = REPO_ROOT / "target" / "public-api"

EXIT_OK = 0
EXIT_DIFF = 1
EXIT_ENV = 2

HEADER = """\
# Public API surface of `{package}` -- generated, do not edit.
#
# Regenerate with `python3 scripts/check-public-api.py --write`. The pinned nightly, the pinned
# `cargo-public-api` version, and the flags it is invoked with all live in
# `[workspace.metadata.public-api]` in the root `Cargo.toml`; this file deliberately repeats
# none of them, because a version recorded in two places is a version that drifts. CI runs this
# same script, so a surface change that skips the regeneration fails there.
"""


class EnvironmentProblem(Exception):
    """A toolchain or tooling fault -- deliberately not a surface difference."""


def load_pins() -> dict:
    with MANIFEST.open("rb") as fh:
        return tomllib.load(fh)["workspace"]["metadata"]["public-api"]


def tool_root(pins: dict) -> Path:
    # The pin is encoded in the directory name, which is what makes "is this the pinned build?"
    # answerable at all. `cargo-public-api --version` cannot answer it: 0.52.0 depends on
    # `public-api ^0.52.0`, whose 0.52.0 and 0.52.2 speak different rustdoc JSON formats, so a
    # locked and an unlocked install report the same string and accept different nightlies.
    # Encoding it here also makes a bumped pin a cache miss rather than a stale hit, and keeps
    # a developer's own `~/.cargo/bin` install neither used nor overwritten.
    stamp = f"{pins['tool']}-{pins['tool-version']}"
    if pins["locked"]:
        stamp += "-locked"
    cache = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
    return cache / "outrig" / "public-api-tools" / stamp


def rustc_report(toolchain: str) -> tuple[str, str] | None:
    """The toolchain's `rustc -vV` version line and host triple, or None if not installed."""
    proc = subprocess.run(
        ["rustup", "run", toolchain, "rustc", "-vV"],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        return None
    lines = proc.stdout.splitlines()
    host = next((line.removeprefix("host: ") for line in lines if line.startswith("host: ")), "")
    return lines[0].strip(), host


def ensure_host(pins: dict) -> None:
    want = pins["host-os"]
    have = platform.system().lower()
    if have != want:
        raise EnvironmentProblem(
            f"the snapshots are generated on {want}; this is {have}. `crates/outrig` does not "
            f"compile off {want}, so there is no surface here to render."
        )


def ensure_toolchain(pins: dict, install_missing: bool) -> str:
    """Assert the pinned toolchain is the recorded one, and return its host triple."""
    name, want = pins["toolchain"], pins["toolchain-rustc"]
    command = ["rustup", "toolchain", "install", name, "--profile", "minimal"]
    report = rustc_report(name)
    have = report[0] if report else None
    if have is None:
        if not install_missing:
            raise EnvironmentProblem(
                f"the pinned toolchain `{name}` is not installed. Install it with\n"
                f"    {' '.join(command)}\n"
                f"  or re-run with --install-missing."
            )
        # Installed explicitly, and before anything sets `RUSTUP_TOOLCHAIN`: rustup will
        # otherwise auto-install a named-but-absent toolchain mid-build, with the default
        # profile rather than this one.
        subprocess.run(command, check=False)
        report = rustc_report(name)
        have = report[0] if report else None
        if have is None:
            raise EnvironmentProblem(f"`{' '.join(command)}` failed; its output is above.")
    if have != want:
        raise EnvironmentProblem(
            f"the pinned toolchain `{name}` reports\n"
            f"    {have}\n"
            f"  but `[workspace.metadata.public-api]` records\n"
            f"    {want}\n"
            f"  This is a tooling mismatch, not a surface change. A dated channel is "
            f"immutable, so this usually means the name is shadowed by a `rustup toolchain "
            f"link`, or that the record was not refreshed when the date was bumped."
        )
    if not report[1]:
        raise EnvironmentProblem(f"`rustup run {name} rustc -vV` named no host triple")
    return report[1]


def ensure_tool(pins: dict, root: Path, install_missing: bool) -> Path:
    binary = root / "bin" / pins["tool"]
    if binary.exists():
        return binary
    spec = f"{pins['tool']}@{pins['tool-version']}"
    command = ["cargo", "install", spec, "--root", str(root)]
    if pins["locked"]:
        command.append("--locked")
    if not install_missing:
        raise EnvironmentProblem(
            f"the pinned `{pins['tool']}` is not installed at {binary}. Install it with\n"
            f"    {' '.join(command)}\n"
            f"  or re-run with --install-missing."
        )
    # `RUSTUP_TOOLCHAIN` is dropped here on purpose: the pinned nightly is what reads this
    # crate's rustdoc, not what should compile the tool itself.
    env = {k: v for k, v in os.environ.items() if k != "RUSTUP_TOOLCHAIN"}
    if subprocess.run(command, env=env, check=False).returncode != 0 or not binary.exists():
        raise EnvironmentProblem(
            f"`{' '.join(command)}` failed; its output is above. `--locked` takes that "
            f"version's own lockfile, so a yanked transitive dependency fails here rather "
            f"than resolving to something that speaks a different rustdoc JSON format. The "
            f"fix is to move the pins in `[workspace.metadata.public-api]` together."
        )
    return binary


def generate(pins: dict, binary: Path, host: str, package: str) -> str:
    command = [
        str(binary),
        "public-api",
        "--manifest-path",
        str(MANIFEST),
        "--package",
        package,
        # The pinned toolchain's *own* host triple, so this is never a cross-build -- but named
        # explicitly, because it has to be. Left off, cargo still honors an ambient
        # `CARGO_BUILD_TARGET` or a `build.target` in any config file and writes the rustdoc
        # JSON under `target/public-api/<triple>/doc`, while the tool computes the unqualified
        # `target/public-api/doc` to read back. On a warm tree that is a silent false pass: the
        # previous run's JSON is read and a changed surface reports as unchanged. An explicit
        # `--target` makes both sides agree, and outranks both settings.
        "--target",
        host,
        # Not optional: the tool writes ANSI into a pipe under its default `auto`, which would
        # land escape codes in a committed file.
        "--color=never",
    ]
    if pins["omit"]:
        command += ["--omit", ",".join(pins["omit"])]
    if pins["no-default-features"]:
        command.append("--no-default-features")
    if pins["features"]:
        command += ["--features", ",".join(pins["features"])]

    env = dict(os.environ)
    # The whole pin rides on this variable. Without it the tool falls back to whatever the
    # `nightly` alias happens to point at, which is a pin that fails open.
    env["RUSTUP_TOOLCHAIN"] = pins["toolchain"]
    env["CARGO_TARGET_DIR"] = str(TARGET_DIR)
    # rustdoc reads rmeta, so nothing here ever reads the debug info the dev profile emits --
    # and what does get codegen'd (proc macros, build scripts) is the bulk of what CI caches.
    env["CARGO_PROFILE_DEV_DEBUG"] = "0"

    # stdout is the surface; stderr is the tool's own (colored, unstable) logging and is left
    # to flow through unread.
    proc = subprocess.run(command, env=env, cwd=REPO_ROOT, stdout=subprocess.PIPE, text=True)
    if proc.returncode != 0:
        raise EnvironmentProblem(
            f"`{pins['tool']}` exited {proc.returncode} for `{package}`; its output is above. "
            f"If it names a rustdoc JSON format version, the pinned toolchain and the pinned "
            f"tool have drifted apart -- a tooling fault, not a surface change. Both pins live "
            f"in `[workspace.metadata.public-api]` and move together."
        )
    if not proc.stdout.strip():
        raise EnvironmentProblem(
            f"`{pins['tool']}` produced no output for `{package}`. Treating that as a surface "
            f"would read as though every item had been deleted."
        )
    return HEADER.format(package=package) + proc.stdout


def compare(committed: str, generated: str, name: str) -> list[str]:
    """The unified diff between a committed snapshot and a freshly generated one."""
    diff = difflib.unified_diff(
        committed.splitlines(keepends=True),
        generated.splitlines(keepends=True),
        fromfile=f"{name} (committed)",
        tofile=f"{name} (generated)",
    )
    return [line.rstrip("\n") for line in diff]


def report(label: str, violations: list[str]) -> bool:
    if violations:
        print(f"{label}:")
        for v in violations:
            print(f"  {v}")
        return False
    print(f"{label}: OK")
    return True


def self_test(pins: dict) -> bool:
    """Prove `compare` rejects both directions of drift, and names what moved.

    Driven off a real committed snapshot rather than a synthetic fixture, so the assertion is
    about the surface this repo actually has. A check that only catches removals would let the
    snapshot drift in the direction it really drifts, which is additive.
    """
    entry = pins["crates"][0]
    name = entry["snapshot"]
    committed = (REPO_ROOT / name).read_text(encoding="utf-8")
    lines = committed.splitlines(keepends=True)

    index = next((i for i, line in enumerate(lines) if line.startswith("pub fn ")), None)
    if index is None:
        return report("Self-test", [f"{name} holds no `pub fn` line to mutate"])
    victim = lines[index].rstrip("\n")
    added = "pub fn outrig::__self_test::added() -> ()"

    failures: list[str] = []

    removed_diff = compare(committed, "".join(lines[:index] + lines[index + 1 :]), name)
    if f"-{victim}" not in removed_diff:
        failures.append(f"removing {victim!r} was not reported as a removal")

    added_diff = compare(
        committed, "".join(lines[: index + 1] + [added + "\n"] + lines[index + 1 :]), name
    )
    if f"+{added}" not in added_diff:
        failures.append(f"adding {added!r} was not reported as an addition")

    if compare(committed, committed, name):
        failures.append("an unmodified snapshot was reported as differing")

    return report("Self-test", failures)


def run(args: argparse.Namespace) -> int:
    """The whole check. Returns EXIT_OK or EXIT_DIFF; every fault raises instead."""
    pins = load_pins()

    # A broken comparison is a tooling fault, not a surface difference: this mode never looks
    # at the surface at all.
    if args.self_test:
        if not self_test(pins):
            raise EnvironmentProblem("the snapshot comparison does not report what it should")
        return EXIT_OK

    ensure_host(pins)
    host = ensure_toolchain(pins, args.install_missing)
    binary = ensure_tool(pins, tool_root(pins), args.install_missing)
    # Every crate is generated before any is compared or written, so a fault part way through
    # never half-writes the tree, and a two-crate drift is reported in one run.
    surfaces = [generate(pins, binary, host, e["package"]) for e in pins["crates"]]

    if args.write:
        for entry, generated in zip(pins["crates"], surfaces):
            (REPO_ROOT / entry["snapshot"]).write_text(generated, encoding="utf-8")
            print(f"{entry['snapshot']}: written")
        return EXIT_OK

    for entry in pins["crates"]:
        if not (REPO_ROOT / entry["snapshot"]).exists():
            raise EnvironmentProblem(
                f"{entry['snapshot']} does not exist. A snapshot that has gone missing is not "
                f"a surface difference; a crate newly added to the table is bootstrapped with "
                f"--write."
            )

    # Listed, not generator-fed: `all` would short-circuit, and every crate has to report.
    ok = all(
        [
            report(
                entry["snapshot"],
                compare(
                    (REPO_ROOT / entry["snapshot"]).read_text(encoding="utf-8"),
                    generated,
                    entry["snapshot"],
                ),
            )
            for entry, generated in zip(pins["crates"], surfaces)
        ]
    )
    if not ok:
        print(
            "\nThe surface moved but the snapshot did not. Regenerate with\n"
            "    python3 scripts/check-public-api.py --write\n"
            "and review the diff as part of the change."
        )
    return EXIT_OK if ok else EXIT_DIFF


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--write",
        action="store_true",
        help="regenerate the snapshots in place instead of diffing them",
    )
    mode.add_argument(
        "--self-test",
        action="store_true",
        help="check that the comparison rejects a removed and an added item, and names it",
    )
    parser.add_argument(
        "--install-missing",
        action="store_true",
        help="install the pinned toolchain and tool when they are absent, rather than failing",
    )
    args = parser.parse_args()

    # EXIT_DIFF is returned from exactly one place, in `run`. Everything else -- a mistyped key
    # in the pin table, a manifest that will not parse, a missing `rustup` -- lands here and
    # exits 2, so "the surface moved" is never said by accident about a broken tool.
    try:
        return run(args)
    except EnvironmentProblem as exc:
        print(f"error: {exc}", file=sys.stderr)
    except FileNotFoundError as exc:
        print(f"error: {exc.filename or exc} is not on PATH", file=sys.stderr)
    except Exception:
        traceback.print_exc()
    return EXIT_ENV


if __name__ == "__main__":
    sys.exit(main())
