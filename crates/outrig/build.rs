//! Compile the `outrig-enter` launcher (see `src/container/enter/`) into a
//! statically linked musl binary and hand it to `include_bytes!` via `OUT_DIR`.
//!
//! A direct `rustc` invocation on the single launcher file -- no `cargo`, no
//! dependencies -- so it needs only `rustup target add <arch>-unknown-linux-musl`
//! (musl targets ship their own crt objects and `libc.a`), no C toolchain, and
//! it cannot deadlock on cargo's package-cache/workspace locks.
//!
//! The output is always a Linux binary: the helper runs inside the container,
//! not in the host process. Selection is therefore by target architecture, not
//! target OS. OutRig itself is Linux-only today -- `nsfork` and `network` call
//! `setns` unconditionally -- so the two coincide; the arch-shaped rule is what
//! would keep this correct if that ever changed.
//!
//! When the target is absent the build still succeeds with an empty artifact;
//! the feature degrades to a runtime error whose hint resolves it, because the
//! launcher source ships inside this (published) crate.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

const LAUNCHER: &str = "src/container/enter/launcher.rs";
/// The launcher and everything it `include!`s, watched as a directory: cargo
/// scans it recursively, so a new pure helper needs no second registration
/// here. Listing files one by one would put a silent failure mode in the way --
/// forget one and a stale binary stays embedded with nothing to say so.
const LAUNCHER_DIR: &str = "src/container/enter";
/// Set to make the graceful degradation below a hard build error instead. CI
/// sets it: every degradation path is a warning plus an empty artifact, and the
/// test for the embedded launcher has a matching early-return, so a launcher
/// that stopped compiling would otherwise be a green build.
const REQUIRE_ENTER: &str = "OUTRIG_REQUIRE_ENTER";
/// Carries the degradation reason to `error.rs`, which reads it with
/// `option_env!` under this same name -- emitted only when degrading, so
/// "absent" is the success case rather than a sentinel value. Keep the two
/// spellings in step.
const REASON_ENV: &str = "OUTRIG_ENTER_UNAVAILABLE_REASON";

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
    // a rebuild on its own: cargo reruns a build script when a `rerun-if-changed`
    // path appears or changes, and `--print target-libdir` names the path even
    // before the target is installed. Without this the "run rustup target add and
    // rebuild" hint would be a lie -- no tracked input changes when a target is
    // installed, so a plain rebuild would keep the empty artifact.
    let libdir = target_libdir(&rustc, triple);
    if let Some(dir) = &libdir {
        println!("cargo:rerun-if-changed={dir}");
    }
    if !libdir.as_deref().is_some_and(|d| Path::new(d).is_dir()) {
        unavailable(
            &dest,
            &format!(
                "target `{triple}` is not installed -- run `rustup target add {triple}` \
                 and rebuild"
            ),
        );
        return;
    }

    let status = Command::new(&rustc)
        .args(["--edition", "2024", "--target", triple])
        // `rust-lld` rather than the default `cc`. What a musl target ships is
        // the crt objects and `libc.a`; the linker *driver* is still the host's,
        // so an x86-64 host building the AArch64 helper hands foreign-arch
        // objects to the host `ld` and gets "Relocations in generic ELF (EM:
        // 183)". Native builds link either way -- only the cross does not, so
        // `.github/workflows/ci.yml` cross-compiles the launcher to keep this
        // honest on an x86-64-only runner.
        .args(["-C", "linker=rust-lld"])
        .args([
            "-C",
            "opt-level=2",
            "-C",
            "panic=abort",
            "-C",
            "strip=symbols",
        ])
        .arg("-o")
        .arg(&dest)
        .arg(LAUNCHER)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => unavailable(
            &dest,
            &format!("compiling the filesystem-view helper failed ({s})"),
        ),
        Err(e) => unavailable(
            &dest,
            &format!("could not run rustc for the filesystem-view helper: {e}"),
        ),
    }
}

/// The musl target's sysroot lib dir per `rustc --print target-libdir`, which
/// names the path even when the target is not installed (the dir just does not
/// exist yet). `None` only when rustc itself could not be run.
fn target_libdir(rustc: &OsStr, triple: &str) -> Option<String> {
    let out = Command::new(rustc)
        .args(["--print", "target-libdir", "--target", triple])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
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
    println!("cargo:warning=outrig: {msg}; view=\"primary\" sidecars will be unavailable");
    println!("cargo:rustc-env={REASON_ENV}={msg}");
    std::fs::write(dest, []).expect("write empty helper artifact");
}
