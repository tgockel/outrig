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
//!
//! It also fetches the static CPython every session mounts, verifies it against
//! the pin below, and hands it to `include_bytes!` the same way (see
//! `src/python/`). A plain `cargo build` is the whole setup: the archive is
//! downloaded once per machine into the user's cache, and a build that cannot
//! reach it degrades exactly as a missing musl target does.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// The payload checks: `verify_archive`, and the ELF parser it rests on. See
// `src/python/archive.rs` for why they are shared by `include!`.
include!("src/container/enter/elf.rs");
include!("src/python/archive.rs");

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
    python_payload(Path::new(&out_dir));
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

/// The static CPython every session mounts: `python-build-standalone`'s
/// `+static` build, pinned by release and version. The `+static` variant is the
/// point -- the plain musl builds load a musl runtime the *image* would have to
/// supply, and the image is allowed to have none.
const PY_RELEASE: &str = "20260901";
const PY_VERSION: &str = "3.13.15";
/// Per target architecture: the release's triple, the archive's SHA-256 from
/// the release's own `SHA256SUMS`, and the interpreter's ELF `e_machine`.
const PY_PINS: &[(&str, &str, &str, u16)] = &[
    (
        "x86_64",
        "x86_64-unknown-linux-musl",
        "68606ae38cb3f4db0d0fdb75b16dde78888428161e8f82430915d405b5ea96de",
        62,
    ),
    (
        "aarch64",
        "aarch64-unknown-linux-musl",
        "d6d5838cbda9365dc793b3174264715e601562282609501e591382c2db5ed7b9",
        183,
    ),
];
/// A local copy of the pinned archive, for a build with no network. Verified
/// exactly as a download is.
const PY_ARCHIVE_ENV: &str = "OUTRIG_PYTHON_ARCHIVE";
/// As [`REQUIRE_ENTER`], for the Python payload.
const REQUIRE_PYTHON: &str = "OUTRIG_REQUIRE_PYTHON";
/// As [`REASON_ENV`]; `src/python/payload.rs` reads it back.
const PY_REASON_ENV: &str = "OUTRIG_PYTHON_UNAVAILABLE_REASON";
/// The archive's name less `.tar.zst`, which `src/python/payload.rs` unpacks
/// under: a new pin unpacks beside an old one rather than over it.
const PY_PAYLOAD_ENV: &str = "OUTRIG_PYTHON_PAYLOAD";

/// Fetch, verify, and stage the Python payload as `OUT_DIR/python.tar.zst`.
fn python_payload(out_dir: &Path) {
    println!("cargo:rerun-if-changed=src/python/archive.rs");
    println!("cargo:rerun-if-env-changed={PY_ARCHIVE_ENV}");
    println!("cargo:rerun-if-env-changed={REQUIRE_PYTHON}");
    let dest = out_dir.join("python.tar.zst");

    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let Some(&(_, triple, sha256, e_machine)) = PY_PINS.iter().find(|pin| pin.0 == arch) else {
        println!("cargo:rustc-env={PY_PAYLOAD_ENV}=");
        python_unavailable(
            &dest,
            &format!("no Python payload is pinned for arch {arch:?}"),
        );
        return;
    };
    let payload = format!("cpython-{PY_VERSION}+{PY_RELEASE}-{triple}-noopt+static-full");
    println!("cargo:rustc-env={PY_PAYLOAD_ENV}={payload}");
    let name = format!("{payload}.tar.zst");

    let archive = match fetch_python(&name, sha256, out_dir) {
        Ok(archive) => archive,
        Err(why) => return python_unavailable(&dest, &why),
    };
    // A failure here is not a missing payload but a wrong one, which no build
    // embeds -- whatever `OUTRIG_REQUIRE_PYTHON` says.
    let minor = PY_VERSION
        .rsplit_once('.')
        .map_or(PY_VERSION, |(minor, _)| minor);
    let interpreter = format!("python/install/bin/python{minor}");
    if let Err(why) = verify_archive(&archive, sha256, &interpreter, e_machine) {
        panic!("outrig: refusing the Python payload {name}: {why}");
    }
    std::fs::write(&dest, &archive).expect("write the Python payload");
}

/// The pinned archive's bytes, from [`PY_ARCHIVE_ENV`] if it is set, else from
/// the per-user download cache, else downloaded into it. Whether to trust them
/// is `verify_archive`'s call, not this one's.
fn fetch_python(name: &str, sha256: &str, out_dir: &Path) -> Result<Vec<u8>, String> {
    if let Some(path) = std::env::var_os(PY_ARCHIVE_ENV) {
        let path = PathBuf::from(path);
        return std::fs::read(&path)
            .map_err(|e| format!("cannot read {PY_ARCHIVE_ENV}={}: {e}", path.display()));
    }
    // Once per machine, not once per target directory, profile, and feature
    // set -- each of which gets its own `OUT_DIR`, the fallback.
    let cache = download_cache().filter(|dir| std::fs::create_dir_all(dir).is_ok());
    let dirs: Vec<&Path> = cache
        .iter()
        .map(PathBuf::as_path)
        .chain([out_dir])
        .collect();
    if let Some(cached) = cached_archive(&dirs, name, sha256) {
        return Ok(cached);
    }

    // Written aside and renamed, so an interrupted download is never mistaken
    // for the archive. The cache is used only if it takes the file: a
    // directory that exists can still be read-only, which no rebuild fixes.
    let partial = format!("{name}.partial-{}", std::process::id());
    let (dir, file) = dirs
        .iter()
        .find_map(|&dir| {
            let file = std::fs::File::create(dir.join(&partial)).ok()?;
            Some((dir, file))
        })
        .ok_or_else(|| format!("cannot write {partial} to the download cache or OUT_DIR"))?;
    let url = format!(
        "https://github.com/astral-sh/python-build-standalone/releases/download/{PY_RELEASE}/{name}"
    );
    let path = dir.join(name);
    let downloaded = download(&url, file)
        .and_then(|()| std::fs::rename(dir.join(&partial), &path).map_err(|e| e.to_string()));
    if let Err(e) = downloaded {
        let _ = std::fs::remove_file(dir.join(&partial));
        return Err(format!(
            "cannot download {url}: {e} -- rebuild with network access, or set \
             {PY_ARCHIVE_ENV} to a copy of {name}"
        ));
    }
    std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// `$XDG_CACHE_HOME/outrig/downloads`, or `$HOME/.cache/outrig/downloads`:
/// `XDG_CACHE_HOME` only when absolute, the rule `src/python/payload.rs`'s
/// `cache_root` applies to where sessions unpack.
fn download_cache() -> Option<PathBuf> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })?;
    Some(root.join("outrig/downloads"))
}

/// Stream `url` into `file`, following the environment's proxy settings.
fn download(url: &str, mut file: std::fs::File) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(15 * 60)))
        .build()
        .into();
    let response = agent.get(url).call().map_err(|e| e.to_string())?;
    std::io::copy(&mut response.into_body().into_reader(), &mut file)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// As [`unavailable`], for the Python payload: sessions then fail at start with
/// `msg`. Also names a path that never exists as an input, which makes cargo
/// rerun this script on every build until one succeeds -- the usual remedy is
/// simply to rebuild with network, and nothing else it tracks would change.
fn python_unavailable(dest: &Path, msg: &str) {
    if std::env::var_os(REQUIRE_PYTHON).is_some() {
        panic!("outrig: {msg} ({REQUIRE_PYTHON} is set)");
    }
    println!("cargo:warning=outrig: {msg}; sessions will not start until a build embeds Python");
    println!("cargo:rustc-env={PY_REASON_ENV}={msg}");
    println!(
        "cargo:rerun-if-changed={}",
        dest.with_extension("retry").display()
    );
    std::fs::write(dest, []).expect("write empty Python payload");
}
