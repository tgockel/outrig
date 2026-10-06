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
//! `src/python/`), and does the same for each pure-Python wheel in [`WHEELS`],
//! unpacked beside the payload. A plain `cargo build` is the whole setup: each
//! archive is downloaded once per machine into the user's cache, and a build
//! that cannot reach one degrades exactly as a missing musl target does.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// The payload and wheel checks: `verify_archive`, `verify_digest`, `wheel_to_tar`,
// and the ELF parser they rest on. See `src/python/archive.rs` for why they are
// shared by `include!`.
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
    wheels(Path::new(&out_dir));
    programs(Path::new(&out_dir));
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
/// As [`REQUIRE_ENTER`], for the Python payload and the wheels beside it.
const REQUIRE_PYTHON: &str = "OUTRIG_REQUIRE_PYTHON";
/// As [`REASON_ENV`]; `src/python/payload.rs` reads it back.
const PY_REASON_ENV: &str = "OUTRIG_PYTHON_UNAVAILABLE_REASON";
/// The archive's name less `.tar.zst`, which `src/python/payload.rs` unpacks
/// under: a new pin unpacks beside an old one rather than over it.
const PY_PAYLOAD_ENV: &str = "OUTRIG_PYTHON_PAYLOAD";

/// A pure-Python wheel the build embeds and a session unpacks beside the
/// payload, as `src/python/payload.rs` describes.
struct Wheel {
    /// The wheel's file name, which is also the download cache's key.
    name: &'static str,
    url: &'static str,
    sha256: &'static str,
    /// A local copy, for a build with no network; as [`PY_ARCHIVE_ENV`].
    local_env: &'static str,
    /// As [`PY_REASON_ENV`].
    reason_env: &'static str,
    /// The name less `.whl`, which is the directory the wheel unpacks to and
    /// the artifact's name under `OUT_DIR`; `payload.rs` reads it back.
    dir_env: &'static str,
}

/// Every wheel embedded, one row per pin. RPyC carries hosted-object requests
/// between the interpreter and each binding process; its one declared
/// dependency, `plumbum`, is used only by its command-line tools, so it is not
/// pinned here.
const WHEELS: &[Wheel] = &[Wheel {
    name: "rpyc-6.0.2-py3-none-any.whl",
    url: "https://files.pythonhosted.org/packages/3f/99/\
          2e119d541d596daea39643eb9312b47c7847383951300f889166938035b1/rpyc-6.0.2-py3-none-any.whl",
    sha256: "8072308ad30725bc281c42c011fc8c922be15f3eeda6eafb2917cafe1b6f00ec",
    local_env: "OUTRIG_RPYC_WHEEL",
    reason_env: "OUTRIG_RPYC_UNAVAILABLE_REASON",
    dir_env: "OUTRIG_RPYC_DIR",
}];

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
    let url = format!(
        "https://github.com/astral-sh/python-build-standalone/releases/download/{PY_RELEASE}/{name}"
    );

    let archive = match fetch_artifact(&url, &name, sha256, PY_ARCHIVE_ENV, out_dir) {
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

/// Fetch, verify, and stage each of [`WHEELS`] as `OUT_DIR/<dir>.tar.zst`: the
/// wheel's members inflated and laid out under `<dir>/`, so the runtime unpacks
/// it exactly as it unpacks the payload.
fn wheels(out_dir: &Path) {
    for wheel in WHEELS {
        println!("cargo:rerun-if-env-changed={}", wheel.local_env);
        let dir = wheel
            .name
            .strip_suffix(".whl")
            .expect("a wheel's file name ends in .whl");
        println!("cargo:rustc-env={}={dir}", wheel.dir_env);
        let dest = out_dir.join(format!("{dir}.tar.zst"));

        let bytes = match fetch_artifact(
            wheel.url,
            wheel.name,
            wheel.sha256,
            wheel.local_env,
            out_dir,
        ) {
            Ok(bytes) => bytes,
            Err(why) => {
                artifact_unavailable(
                    &dest,
                    wheel.reason_env,
                    &why,
                    &format!(
                        "hosted objects will be unavailable until a build embeds {}",
                        wheel.name
                    ),
                );
                continue;
            }
        };
        // As for the payload: a wheel that fails here is the wrong one, not a
        // missing one, and no build embeds it.
        let tar = verify_digest(&bytes, wheel.sha256)
            .and_then(|()| wheel_to_tar(&bytes, dir))
            .unwrap_or_else(|why| panic!("outrig: refusing the wheel {}: {why}", wheel.name));
        let compressed = ruzstd::encoding::compress_to_vec(
            &tar[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        std::fs::write(&dest, compressed).expect("write the wheel's tar");
    }
}

/// The Python programs passed to the payload's interpreter as its `-c`
/// argument, by the name of the bootstrap `OUT_DIR` gets for each.
const PROGRAMS: &[(&str, &str)] = &[
    ("interpreter", "src/python/interpreter.py"),
    ("binding", "src/python/binding.py"),
];

/// Linux caps one argument string at 32 pages, and the interpreter's source
/// is past that.
const ARG_STRLEN_MAX: usize = 128 * 1024;

/// Stage each program as the one-line `-c` argument that runs it: its source
/// compressed and base64-encoded behind a bootstrap that inflates, compiles
/// and executes it in `__main__`, exactly as the source itself would run.
fn programs(out_dir: &Path) {
    for (name, path) in PROGRAMS {
        println!("cargo:rerun-if-changed={path}");
        let source = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&source, 9);
        let bootstrap = format!(
            "import base64,zlib;exec(compile(zlib.decompress(base64.b64decode(\"{}\")),\
             \"<{name}>\",\"exec\"))",
            base64(&compressed)
        );
        assert!(
            bootstrap.len() < ARG_STRLEN_MAX,
            "outrig: {path} is {} bytes as a -c argument, past the {ARG_STRLEN_MAX} Linux allows \
             one argument; it has to shrink, or travel as a file",
            bootstrap.len()
        );
        let dest = out_dir.join(format!("{name}.bootstrap"));
        std::fs::write(&dest, bootstrap)
            .unwrap_or_else(|e| panic!("write {}: {e}", dest.display()));
    }
}

/// Standard base64 with padding, as Python's `base64.b64decode` reads it.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits =
            chunk.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((bits >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The pinned artifact's bytes, from `local_env` if it is set, else from the
/// per-user download cache, else downloaded from `url` into it. Whether to
/// trust them is the caller's verification's call, not this one's.
fn fetch_artifact(
    url: &str,
    name: &str,
    sha256: &str,
    local_env: &str,
    out_dir: &Path,
) -> Result<Vec<u8>, String> {
    if let Some(path) = std::env::var_os(local_env) {
        let path = PathBuf::from(path);
        return std::fs::read(&path)
            .map_err(|e| format!("cannot read {local_env}={}: {e}", path.display()));
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
    let path = dir.join(name);
    let downloaded = download(url, file)
        .and_then(|()| std::fs::rename(dir.join(&partial), &path).map_err(|e| e.to_string()));
    if let Err(e) = downloaded {
        let _ = std::fs::remove_file(dir.join(&partial));
        return Err(format!(
            "cannot download {url}: {e} -- rebuild with network access, or set {local_env} to \
             a copy of {name}"
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
/// `msg`.
fn python_unavailable(dest: &Path, msg: &str) {
    artifact_unavailable(
        dest,
        PY_REASON_ENV,
        msg,
        "sessions will not start until a build embeds Python",
    );
}

/// As [`unavailable`], for the payload and the wheels, with `consequence` on
/// the cargo warning and `msg` carried to the runtime in `reason_env`. Also
/// names a path that never exists as an input, which makes cargo rerun this
/// script on every build until one succeeds -- the usual remedy is simply to
/// rebuild with network, and nothing else it tracks would change.
fn artifact_unavailable(dest: &Path, reason_env: &str, msg: &str, consequence: &str) {
    if std::env::var_os(REQUIRE_PYTHON).is_some() {
        panic!("outrig: {msg} ({REQUIRE_PYTHON} is set)");
    }
    println!("cargo:warning=outrig: {msg}; {consequence}");
    println!("cargo:rustc-env={reason_env}={msg}");
    println!(
        "cargo:rerun-if-changed={}",
        dest.with_extension("retry").display()
    );
    std::fs::write(dest, []).expect("write an empty artifact");
}
