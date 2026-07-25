//! Compile the `outrig-enter` launcher (see `src/container/enter/`) into a
//! statically linked musl binary and hand it to `include_bytes!` via `OUT_DIR`.
//!
//! A direct `rustc` invocation on the single launcher file -- no `cargo`, no
//! dependencies -- so it needs only `rustup target add <arch>-unknown-linux-musl`
//! (Rust's musl targets are self-contained and link with `rust-lld`), no C
//! toolchain, and it cannot deadlock on cargo's package-cache/workspace locks.
//! When the target is absent the build still succeeds with an empty artifact;
//! the feature degrades to a runtime error whose hint resolves it, because the
//! launcher source ships inside this (published) crate.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

const LAUNCHER: &str = "src/container/enter/launcher.rs";
const ELF: &str = "src/container/enter/elf.rs";

fn main() {
    println!("cargo:rerun-if-changed={LAUNCHER}");
    println!("cargo:rerun-if-changed={ELF}");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR is set for build scripts");
    let dest = Path::new(&out_dir).join("outrig-enter");

    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let triple = match arch.as_str() {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other => {
            unavailable(
                &dest,
                &format!(
                    "filesystem-view helper is unsupported on arch {other:?}; \
                     view=\"primary\" sidecars will be unavailable"
                ),
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
                "target `{triple}` is not installed; view=\"primary\" sidecars will be \
                 unavailable -- run `rustup target add {triple}` and rebuild"
            ),
        );
        return;
    }

    let status = Command::new(&rustc)
        .args(["--edition", "2024", "--target", triple])
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
            &format!(
                "compiling the filesystem-view helper failed ({s}); \
                 view=\"primary\" sidecars will be unavailable"
            ),
        ),
        Err(e) => unavailable(
            &dest,
            &format!(
                "could not run rustc for the filesystem-view helper: {e}; \
                 view=\"primary\" sidecars will be unavailable"
            ),
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
fn unavailable(dest: &Path, msg: &str) {
    println!("cargo:warning=outrig: {msg}");
    std::fs::write(dest, []).expect("write empty helper artifact");
}
