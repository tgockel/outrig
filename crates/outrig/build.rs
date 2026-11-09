//! Compile the `outrig-enter` launcher (see `src/container/enter/`) into a
//! statically linked binary and hand it to `include_bytes!` via `OUT_DIR`.
//!
//! The launcher is a sidecar's ENTRYPOINT, so it runs in an image OutRig does
//! not control, and it has to be static: a dynamic one needs a loader, and a
//! libc new enough for its symbol versions, from that image. Two builds give a
//! static binary, tried in order:
//!
//! 1. `<arch>-unknown-linux-musl`, which needs only `rustup target add` (musl
//!    targets ship their own crt objects and `libc.a`) and no C toolchain. CI
//!    and releases build this one.
//! 2. The host's own target with `-C target-feature=+crt-static`: on a gnu
//!    host, glibc linked in statically. It needs the host's C linker and its
//!    static C library -- Debian and Ubuntu ship `libc.a` in `libc6-dev`,
//!    beside the crt objects any gnu link needs, and Fedora splits it into
//!    `glibc-static`. A native build only: for a cross build the host's
//!    target is the wrong architecture.
//!
//! Each is a direct `rustc` invocation on the single launcher file -- no
//! `cargo`, no dependencies -- so it cannot deadlock on cargo's
//! package-cache/workspace locks.
//!
//! The output is always a Linux binary: the helper runs inside the container,
//! not in the host process. Selection is therefore by target architecture, not
//! target OS. OutRig itself is Linux-only today -- `nsfork` and `network` call
//! `setns` unconditionally -- so the two coincide; the arch-shaped rule is what
//! would keep this correct if that ever changed.
//!
//! When neither build works the crate still builds, with an empty artifact;
//! the feature degrades to a runtime error whose hint resolves it, because the
//! launcher source ships inside this (published) crate.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use elf::{ElfKind, elf_interp};

// The launcher's own ELF parser, for `elf_interp`: the host fallback counts only
// if it really came out static. The rest of the file serves the launcher, so it
// is dead code here, and its `#[cfg(test)]` tests are inert.
#[allow(dead_code)]
mod elf {
    include!("src/container/enter/elf.rs");
}

const LAUNCHER: &str = "src/container/enter/launcher.rs";
/// The launcher and everything it `include!`s, watched as a directory: cargo
/// scans it recursively, so a new pure helper needs no second registration
/// here. Listing files one by one would put a silent failure mode in the way --
/// forget one and a stale binary stays embedded with nothing to say so.
const LAUNCHER_DIR: &str = "src/container/enter";
/// Set to make a missing musl build a hard build error instead of a fallback
/// or a degradation. CI sets it: every degradation path is a warning plus an
/// empty artifact, and the test for the embedded launcher has a matching
/// early-return, so a launcher that stopped compiling would otherwise be a
/// green build. It asks for the musl build in particular -- see `main`.
const REQUIRE_ENTER: &str = "OUTRIG_REQUIRE_ENTER";
/// Carries the degradation reason to `error.rs`, which reads it with
/// `option_env!` under this same name -- emitted only when degrading, so
/// "absent" is the success case rather than a sentinel value. Keep the two
/// spellings in step.
const REASON_ENV: &str = "OUTRIG_ENTER_UNAVAILABLE_REASON";
/// Where both warnings send a build that has no musl target.
const NO_MUSL_DOC: &str = "https://tgockel.github.io/outrig/usage/no-musl.html";

fn main() {
    println!("cargo:rerun-if-changed={LAUNCHER_DIR}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={REQUIRE_ENTER}");

    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR is set for build scripts");
    let dest = Path::new(&out_dir).join("outrig-enter");

    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let triple = match arch.as_str() {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other => {
            unavailable(
                &dest,
                &format!("filesystem-view helper is unsupported on arch {other:?}"),
            );
            return;
        }
    };

    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());

    // Track the target's sysroot lib dir so a later `rustup target add` triggers
    // a rebuild on its own. Without this the "run rustup target add and rebuild"
    // hint would be a lie -- no tracked input changes when a target is installed,
    // so a plain rebuild would keep the empty or fallback artifact. But cargo
    // counts a watched path that does not exist as changed on every build, which
    // reruns this script and recompiles this crate and all its dependents each
    // time. So a missing lib dir is watched through `<sysroot>/lib/rustlib`: that
    // exists, and `rustup target add` creates the target's directory in it,
    // which changes its mtime (cargo scans a watched directory recursively, the
    // directory's own mtime included).
    let libdir = target_libdir(&rustc, triple);
    let installed = libdir.as_deref().is_some_and(Path::is_dir);
    if let Some(dir) = libdir.as_deref() {
        let watch = match dir.parent().and_then(Path::parent) {
            Some(rustlib) if !installed && rustlib.is_dir() => rustlib,
            _ => dir,
        };
        println!("cargo:rerun-if-changed={}", watch.display());
    }

    let musl_reason = if installed {
        // `rust-lld` rather than the default `cc`. What a musl target ships is
        // the crt objects and `libc.a`; the linker *driver* is still the host's,
        // so an x86-64 host building the AArch64 helper hands foreign-arch
        // objects to the host `ld` and gets "Relocations in generic ELF (EM:
        // 183)". Native builds link either way -- only the cross does not, so
        // `.github/workflows/ci.yml` cross-compiles the launcher to keep this
        // honest on an x86-64-only runner.
        match compile(&rustc, triple, "linker=rust-lld", &dest) {
            Ok(()) => return,
            Err(e) => format!("compiling the filesystem-view helper for `{triple}` failed ({e})"),
        }
    } else {
        format!(
            "target `{triple}` is not installed -- run `rustup target add {triple}` \
             and rebuild"
        )
    };

    // Second choice, a static build for the host triple: it only runs in this
    // target's containers when the host is Linux on the same arch. Never under
    // `REQUIRE_ENTER`, where `unavailable` panics with the musl reason instead --
    // CI checks the musl build, and a fallback would mask its failure.
    let host = std::env::var("HOST").unwrap_or_default();
    if std::env::var_os(REQUIRE_ENTER).is_some()
        || host.split('-').next() != Some(arch.as_str())
        || !host.contains("-linux-")
    {
        unavailable(&dest, &musl_reason);
        return;
    }

    // The default linker, not `rust-lld`: a static glibc link needs the host
    // `cc` to find the crt objects and `libc.a`. And rustc silently ignores
    // `+crt-static` on a target that cannot honor it, so only the output shows
    // whether the link really came out static.
    let built = compile(&rustc, &host, "target-feature=+crt-static", &dest)
        .map(|()| elf_interp(&std::fs::read(&dest).unwrap_or_default(), |_, _| Vec::new()));
    let why = match built {
        Ok(Ok(ElfKind::Static)) => {
            println!(
                "cargo:warning=outrig: {musl_reason}; built a statically linked `outrig-enter` \
                 for the host target `{host}` instead, which works in any sidecar image -- see \
                 {NO_MUSL_DOC}"
            );
            return;
        }
        Ok(Ok(ElfKind::Dynamic(loader))) => {
            format!("came out dynamically linked (loader {loader}), so it was discarded")
        }
        Ok(Err(e)) => format!("was discarded: {e}"),
        Err(e) => format!(
            "failed too ({e}) -- it needs the host's static C library, `libc.a` (on Fedora, \
             `glibc-static`)"
        ),
    };
    unavailable(
        &dest,
        &format!("{musl_reason}; the static fallback for the host target `{host}` {why}"),
    );
}

/// Compile the launcher for `target` into `dest` with one extra `-C` option.
/// `Err` is rustc's exit status or why it could not be run. CI's cross-compile
/// step repeats the musl command line by hand; keep the two in step.
fn compile(rustc: &OsStr, target: &str, codegen: &str, dest: &Path) -> Result<(), String> {
    let status = Command::new(rustc)
        .args(["--edition", "2024", "--target", target, "-C", codegen])
        .args([
            "-C",
            "opt-level=2",
            "-C",
            "panic=abort",
            "-C",
            "strip=symbols",
        ])
        .arg("-o")
        .arg(dest)
        .arg(LAUNCHER)
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(s.to_string()),
        Err(e) => Err(format!("could not run rustc: {e}")),
    }
}

/// The musl target's sysroot lib dir per `rustc --print target-libdir`, which
/// names the path even when the target is not installed (the dir just does not
/// exist yet). `None` only when rustc itself could not be run.
fn target_libdir(rustc: &OsStr, triple: &str) -> Option<PathBuf> {
    let out = Command::new(rustc)
        .args(["--print", "target-libdir", "--target", triple])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().into())
}

/// Emit the "helper unavailable" warning and leave an empty artifact so the
/// build still succeeds; `is_available()` then reports false at runtime.
///
/// `msg` is cause and remedy only, on one line (cargo directives are
/// line-oriented). The consequence -- which sidecars stop working -- belongs on
/// the cargo warning, where there is no context to infer it from, and not in
/// what `error.rs` renders: a caller reading that has just been told it by the
/// error's first line.
fn unavailable(dest: &Path, msg: &str) {
    if std::env::var_os(REQUIRE_ENTER).is_some() {
        panic!("outrig: {msg} ({REQUIRE_ENTER} is set)");
    }
    println!(
        "cargo:warning=outrig: {msg}; view=\"primary\" sidecars will be unavailable -- see \
         {NO_MUSL_DOC}"
    );
    println!("cargo:rustc-env={REASON_ENV}={msg}");
    std::fs::write(dest, []).expect("write empty helper artifact");
}
