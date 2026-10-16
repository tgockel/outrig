//! `outrig-enter` -- run a program from THIS (sidecar) container's image with
//! ANOTHER container's filesystem view.
//!
//!     outrig-enter [--target PID | --ns-file PATH] [--graft DIR] [--cwd DIR]
//!                  [--uid N --gid N] -- PROGRAM [ARGS...]
//!
//! A Rust port of the prototype's `sidecar-enter.c`
//! (<https://github.com/tgockel/prototype-podman-shared-fs>). It is the sidecar
//! image's ENTRYPOINT, so it runs before any graft exists, against an image
//! OutRig does not control -- hence it is compiled statically for
//! `*-unknown-linux-musl` and depends on nothing but the kernel: it declares
//! the handful of libc symbols and syscall numbers it needs itself, so the
//! embedding `build.rs` can compile it with a single `rustc` invocation.
//!
//! Expected launch (0002-13 arranges it): `--userns=container:<target>` (so we are
//! already in the user namespace that owns the target's mount namespace),
//! `--cap-add=SYS_ADMIN` (setns is gated on it) and `--cap-add=SYS_PTRACE`
//! (to open the target's nsfs file).
//!
//! **The argv contract, which the caller depends on:** `PROGRAM` is in *this*
//! container's coordinates, ungrafted -- it is opened *and resolved to a
//! symlink-free path* before the setns, while this image's rootfs is still at
//! `/`, and the graft is applied to it here when handing the path to the
//! loader. Opening it is not enough on its own: the loader is given the path
//! by name and opens it again, after the setns, when an absolute symlink in it
//! would mean a file of the primary's (see the ordering contract below). For
//! the same reason a *dynamic* `PROGRAM` that is not absolute is refused --
//! there is no working directory left to apply the graft prefix to. A static
//! one is unaffected, having no path to hand anybody. A `PROGRAM` with no `/`
//! in it is searched along this process's own `PATH` first (see
//! `path_search.rs`), which is the sidecar image's -- the same environment the
//! image's `ENTRYPOINT` would have resolved against had podman run it
//! directly. `ARGS...` are in the *target's* coordinates and are passed
//! through untouched.
//! `container::sidecar`'s `build_primary_view_argv` is the producer that honors
//! this; changing either side alone silently breaks the other, and the failure
//! looks like an image that cannot find its own interpreter.
//!
//! It also mounts a fresh `/proc`. The one it inherits belongs to the target's
//! PID namespace, which this process never joined, so `/proc/self` resolves to
//! nothing there and payloads that read it fail obscurely.
//!
//! **The ordering contract, which the privilege drop depends on:** every step
//! up to and including the `chdir` needs `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` --
//! `open_tree`, `open(ns)`, `setns`, `unshare`, `mount`, `move_mount`, and a
//! `--cwd` that may name a directory only root can enter. Nothing after it
//! does, so `--uid`/`--gid` drop to the session's ids there, immediately
//! before the exec. `PROGRAM` is opened at the very top, while this image's
//! rootfs is still at `/` and while still privileged, so the `ElfKind::Static`
//! `execveat` from that descriptor still works afterwards -- the kernel checks
//! exec permission against the file's own mode, which for an image binary is
//! `0755`. Reordering the drop earlier breaks the graft; reordering it later
//! is not possible, because the exec is the last thing this process does.
//! The mask over `/etc/ld.so.preload` is one more `mount`, so it sits before
//! the drop as well.
//!
//! Every path this file *hands* the dynamic exec -- the program, its
//! interpreter, and each `--library-path` entry -- is canonicalized in that
//! same pre-setns phase, and for the same reason rather than as a tidiness
//! pass: after the setns an absolute symlink resolves in the primary's rootfs
//! instead of the graft, which `canon.rs` explains in full. That closes the
//! paths this file names. It does not close the ones the loader goes on to find
//! for itself, which also mean the primary's files after the setns: a
//! `DT_NEEDED` library that is an absolute symlink, the loader's built-in
//! search directories (a library missing from `--library-path` falls through
//! to them), and the entries of musl's `/etc/ld-musl-<arch>.path`. The path
//! file itself is the sidecar's, since musl finds it relative to its own
//! pathname, which is under the graft; see #595.
//!
//! One file the loader reads for itself is handled here (#268), because a mount
//! can cover it: glibc's loader opens `/etc/ld.so.preload` on every start, and
//! after the setns that is the primary's. For a payload whose loader is not
//! musl's, `main` bind-mounts `/dev/null` over it, in the private namespace it
//! already holds. That namespace is the payload's descendants' too, so the
//! primary's own binaries the payload starts see the file empty as well. The
//! mount covers the file as it is when `main` makes it, and only while the
//! primary leaves it alone: the kernel detaches a mount whose file is renamed
//! over or deleted (`sed -i`, say), in every namespace, and a file created
//! later was never covered, so after either a process the payload starts reads
//! the primary's file. The payload itself read it once, at its start.
//!
//! **A dynamic payload cannot re-execute itself, which is a limit of this
//! design and not a bug in it (#358).** The kernel resolves a binary's
//! `PT_INTERP` against the process's root, which after the setns is the
//! primary's, so a dynamic payload can only run by naming the sidecar's loader
//! explicitly. That makes the kernel's `exe` link the loader and leaves
//! `argv[0]` a graft path whose own `PT_INTERP` the kernel will resolve in the
//! primary: a payload that execs either one (Node `fork()`, Python
//! `sys.executable`, Rust `current_exe()`) fails. A static payload has no loader
//! to name and is exec'd from the open file, so executing the literal path
//! `/proc/self/exe` re-runs it; the *paths* fail, since none of the sidecar's
//! rootfs is grafted for it -- `argv[0]` is a path the view lacks, and the exe
//! link reads back as `/`, which `current_exe()` then hands over. Nothing here
//! can close the dynamic case: the root has to stay the primary's, and putting
//! the sidecar's loader where the kernel looks either needs a mount point the
//! primary may not have or replaces the loader the primary's own binaries use.
//! `doc/concepts/mcp-servers.md` carries the user-facing version as "A dynamic
//! payload cannot re-execute itself"; changing the exec strategy means changing
//! that paragraph with it.
//!
//! This file is compiled only by the `outrig` crate's `build.rs`, always for a
//! Linux musl target; it is not part of the normal `cargo build`. The logic it
//! relies on lives in `elf.rs`, `path_search.rs` and `canon.rs`, pulled in
//! below and unit-tested on the host.

use std::ffi::{CString, OsString, c_char, c_int, c_long, c_ulong, c_void};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

include!("canon.rs");
include!("elf.rs");
include!("path_search.rs");

unsafe extern "C" {
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn faccessat(dirfd: c_int, path: *const c_char, mode: c_int, flags: c_int) -> c_int;
    fn pread(fd: c_int, buf: *mut c_void, count: usize, offset: i64) -> isize;
    fn chdir(path: *const c_char) -> c_int;
    fn setns(fd: c_int, nstype: c_int) -> c_int;
    fn unshare(flags: c_int) -> c_int;
    fn mount(
        src: *const c_char,
        target: *const c_char,
        fstype: *const c_char,
        flags: c_ulong,
        data: *const c_void,
    ) -> c_int;
    fn execv(path: *const c_char, argv: *const *const c_char) -> c_int;
    fn setgroups(size: usize, list: *const u32) -> c_int;
    fn setresgid(rgid: u32, egid: u32, sgid: u32) -> c_int;
    fn setresuid(ruid: u32, euid: u32, suid: u32) -> c_int;
    fn syscall(num: c_long, ...) -> c_long;
}

const O_RDONLY: c_int = 0;
const F_OK: c_int = 0;
const AT_FDCWD_INT: c_int = -100;
const AT_FDCWD: c_long = -100;
const AT_EMPTY_PATH: c_long = 0x1000;
const AT_RECURSIVE: c_long = 0x8000;
const OPEN_TREE_CLONE: c_long = 1;
const MOVE_MOUNT_F_EMPTY_PATH: c_long = 0x04;
const CLONE_NEWNS: c_int = 0x0002_0000;
const MS_NOSUID: c_ulong = 2;
const MS_NODEV: c_ulong = 4;
const MS_NOEXEC: c_ulong = 8;
const MS_BIND: c_ulong = 0x1000;
const MS_REC: c_ulong = 0x4000;
const MS_SLAVE: c_ulong = 1 << 19;
const EPERM: i32 = 1;
const EACCES: i32 = 13;
const ENOTDIR: i32 = 20;

#[cfg(target_arch = "x86_64")]
const SYS_OPEN_TREE: c_long = 428;
#[cfg(target_arch = "x86_64")]
const SYS_MOVE_MOUNT: c_long = 429;
#[cfg(target_arch = "x86_64")]
const SYS_EXECVEAT: c_long = 322;
#[cfg(target_arch = "x86_64")]
const MULTIARCH: &str = "x86_64-linux-gnu";

#[cfg(target_arch = "aarch64")]
const SYS_OPEN_TREE: c_long = 428;
#[cfg(target_arch = "aarch64")]
const SYS_MOVE_MOUNT: c_long = 429;
#[cfg(target_arch = "aarch64")]
const SYS_EXECVEAT: c_long = 281;
#[cfg(target_arch = "aarch64")]
const MULTIARCH: &str = "aarch64-linux-gnu";

/// Loader search directories, graft-relative -- the prototype's fixed list
/// (Debian/Ubuntu multiarch, official node's `/usr/local/lib`, Alpine). The
/// multiarch component follows the build arch so aarch64 works too.
fn lib_dirs() -> [String; 7] {
    [
        "/usr/local/lib".to_string(),
        format!("/lib/{MULTIARCH}"),
        format!("/usr/lib/{MULTIARCH}"),
        "/lib64".to_string(),
        "/usr/lib64".to_string(),
        "/lib".to_string(),
        "/usr/lib".to_string(),
    ]
}

/// Whether `path` is here, in whatever rootfs is currently at `/`. Leaves
/// errno as the failed `open` set it, so a caller may `die()` straight after.
fn exists(path: &str) -> bool {
    let path_c = cstr(path.as_bytes());
    let fd = unsafe { open(path_c.as_ptr(), O_RDONLY) };
    if fd < 0 {
        return false;
    }
    unsafe { close(fd) };
    true
}

/// Whether the kernel says there is nothing at `path`: `ENOENT` (no file, or a
/// dangling link), `ENOTDIR` or `ELOOP`. Any other failure is *not* absence --
/// `EACCES` on a directory above it, say -- so a caller that skips work for an
/// absent path does the work instead.
///
/// `exists` cannot answer this: it asks whether *this* process may read the
/// path, and a launcher without `CAP_DAC_OVERRIDE` may be refused a file the
/// payload's user can read (#600). `faccessat` with `F_OK` needs only search
/// permission on the directories and follows a symlink as the loader's own
/// `access` does. Leaves errno as the failed call set it.
fn absent(path: &str) -> bool {
    let path_c = cstr(path.as_bytes());
    if unsafe { faccessat(AT_FDCWD_INT, path_c.as_ptr(), F_OK, 0) } == 0 {
        return false;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(ENOENT | ENOTDIR | ELOOP)
    )
}

/// The `lib_dirs()` this image actually has, canonicalized and deduplicated --
/// still graft-relative, and still in the fixed list's order.
///
/// Canonicalizing is the same rule the program and its interpreter follow: the
/// loader opens these by name after the setns, so a distro whose `/lib` were an
/// *absolute* symlink would silently be offered the primary's libraries. Both
/// Debian's and Ubuntu's are relative, which is the only reason this one is
/// latent. Deduplication then falls out for free, since `lib_dirs()` is a fixed
/// guess covering four distro layouts at once and several of its entries
/// collapse onto one real directory.
///
/// Dropping the absent ones is a separate, deliberate choice -- resolution
/// keeps a missing component verbatim rather than failing, so this is the
/// `absent` probe's doing, not the walk's. Passing a directory the image does not
/// have was harmless; not passing it says what was searched.
///
/// Must run before the setns, for the reason `canonicalize_under_root` gives.
fn present_lib_dirs() -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    for dir in lib_dirs() {
        let canon = canonical_or_die("library path", dir.as_bytes());
        if !absent(&canon) && !dirs.contains(&canon) {
            dirs.push(canon);
        }
    }
    dirs
}

/// Report `step` with the current errno (plus an optional capability hint) and
/// exit. Never unwinds and never touches the target's filesystem.
fn die(step: &str, hint: &str) -> ! {
    let err = std::io::Error::last_os_error();
    eprintln!("outrig-enter: {step}: {err}{hint}");
    std::process::exit(1);
}

fn cstr(bytes: &[u8]) -> CString {
    CString::new(bytes).unwrap_or_else(|_| die("argument contains an interior NUL", ""))
}

/// Resolve `path` against this image's rootfs, or report which path could not
/// be resolved and exit. `role` names it the way the caller thinks of it --
/// "PROGRAM", "interpreter" -- since the path itself is often one the user
/// never wrote.
///
/// Lossy, like every other path this file renders: the result is headed for a
/// `format!` either way.
///
/// Must run before the setns; see the ordering contract in the module docs.
fn canonical_or_die(role: &str, path: &[u8]) -> String {
    // Named here rather than by errno: the "not absolute" rule is the caller's
    // to explain, and `canon.rs`'s constants are its own.
    let hint = match path.starts_with(b"/") {
        true => "",
        false => " (not an absolute path, so the graft point cannot be applied to it)",
    };
    match canonicalize_under_root(b"", path) {
        Ok(canon) => String::from_utf8_lossy(&canon).into_owned(),
        Err(errno) => {
            let err = std::io::Error::from_raw_os_error(errno);
            eprintln!(
                "outrig-enter: resolve {role} {}: {err}{hint}",
                String::from_utf8_lossy(path),
            );
            std::process::exit(1);
        }
    }
}

/// A NULL-terminated `argv`/`envp` array borrowing from `items`.
fn arg_ptrs(items: &[CString]) -> Vec<*const c_char> {
    let mut ptrs: Vec<*const c_char> = items.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    ptrs
}

/// The current environment as `KEY=VALUE` C strings; empty environment is fine.
fn environ_cstrings() -> Vec<CString> {
    std::env::vars_os()
        .filter_map(|(k, v)| {
            let mut kv = k.into_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            CString::new(kv).ok()
        })
        .collect()
}

fn usage() -> ! {
    eprintln!(
        "usage: outrig-enter [--target PID | --ns-file PATH] [--graft DIR] [--cwd DIR] \
         [--uid N --gid N] -- PROGRAM [ARGS...]"
    );
    std::process::exit(2);
}

/// Parse a numeric flag value, or exit with the flag named. The bound is the
/// target type's: `--target` takes a PID, `--uid`/`--gid` refuse negatives.
fn num_arg<T: std::str::FromStr>(flag: &str, value: &OsString) -> T {
    value
        .to_str()
        .and_then(|s| s.parse::<T>().ok())
        .unwrap_or_else(|| {
            eprintln!("outrig-enter: {flag}: not a valid number");
            std::process::exit(2);
        })
}

/// Become `gid`/`uid` for good: supplementary groups first (they survive a uid
/// change on their own), then the group ids, then the user ids -- each `setres*`
/// sets the real, effective *and* saved id, so there is nothing left to switch
/// back to.
///
/// There is deliberately no `capset` here. The kernel clears the permitted,
/// effective and ambient capability sets on a transition away from uid 0, so
/// `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` are gone with the `setresuid` -- for this
/// process and for everything it spawns. Spelling that out because the absence
/// of a capability call is otherwise the first thing a reader will flag.
fn drop_privileges(uid: u32, gid: u32) {
    if unsafe { setgroups(0, std::ptr::null()) } < 0 {
        die("setgroups(0)", "");
    }
    if unsafe { setresgid(gid, gid, gid) } < 0 {
        die(&format!("setresgid({gid})"), "");
    }
    if unsafe { setresuid(uid, uid, uid) } < 0 {
        die(&format!("setresuid({uid})"), "");
    }
}

/// What a dynamic payload's exec needs, every path of it resolved while the
/// sidecar's rootfs is still `/`. A static payload has no equivalent: it execs
/// from a descriptor and resolves nothing at all after the setns, which is why
/// it was never exposed to the graft escape `canon.rs` describes.
struct DynamicLaunch {
    /// `PROGRAM` as the lookup settled it, canonical -- the loader is handed
    /// this by name and opens it itself.
    program: String,
    /// The payload's `PT_INTERP`, canonical and confirmed present.
    loader: String,
    /// [`present_lib_dirs`], for the loader's `--library-path`.
    lib_dirs: Vec<String>,
    /// Whether that loader is musl's, which takes no `--inhibit-cache` and reads
    /// no `/etc/ld.so.preload`, so `main` masks that file for every other one.
    musl: bool,
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();

    // PID 1 is the target's init under --pid=container:, which OutRig does not
    // use -- it passes --ns-file. The default is for a caller that does.
    let mut target: i64 = 1;
    let mut ns_file: Option<OsString> = None;
    let mut graft = OsString::from("/mnt");
    let mut cwd = OsString::from("/");
    let mut uid: Option<u32> = None;
    let mut gid: Option<u32> = None;

    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_os_str();
        let next = args.get(i + 1);
        match (a.to_str(), next) {
            (Some("--target"), Some(v)) => {
                target = num_arg("--target", v);
                i += 2;
            }
            (Some("--ns-file"), Some(v)) => {
                ns_file = Some(v.clone());
                i += 2;
            }
            (Some("--graft"), Some(v)) => {
                graft = v.clone();
                i += 2;
            }
            (Some("--cwd"), Some(v)) => {
                cwd = v.clone();
                i += 2;
            }
            (Some("--uid"), Some(v)) => {
                uid = Some(num_arg("--uid", v));
                i += 2;
            }
            (Some("--gid"), Some(v)) => {
                gid = Some(num_arg("--gid", v));
                i += 2;
            }
            (Some("--"), _) => {
                i += 1;
                break;
            }
            _ => break,
        }
    }
    if i >= args.len() {
        usage();
    }
    // Both or neither: dropping the uid while keeping gid 0 leaves every file
    // the payload creates owned by the container's root group, which is the
    // half-migration this flag pair exists to avoid.
    let drop_to = match (uid, gid) {
        (Some(uid), Some(gid)) => Some((uid, gid)),
        (None, None) => None,
        _ => {
            eprintln!("outrig-enter: --uid and --gid must be given together");
            std::process::exit(2);
        }
    };

    let prog_argv = &args[i..];
    let named = prog_argv[0].as_os_str();

    // Everything needing the sidecar's own filesystem happens before setns --
    // the `PATH` search included, since the program is a file in *this* image.
    let path_var = std::env::var_os("PATH");
    let path_var = path_var.as_deref();
    let opened = program_candidates(named, path_var)
        .into_iter()
        .find_map(|cand| {
            let cand_c = cstr(cand.as_os_str().as_bytes());
            let fd = unsafe { open(cand_c.as_ptr(), O_RDONLY) };
            (fd >= 0).then_some((fd, cand))
        });
    let Some((prog_fd, resolved)) = opened else {
        // errno is the last candidate's -- for a name nothing provides, the
        // ENOENT the un-searched case would have reported anyway. The hint says
        // where we looked, so "no such program" and "no such path" stay apart.
        let hint = searched_path(named, path_var)
            .map(|p| format!(" (not found in PATH={})", p.to_string_lossy()))
            .unwrap_or_default();
        die(&format!("open {}", named.to_string_lossy()), &hint);
    };

    // Read enough of the head to classify; the file header, program headers and
    // any PT_INTERP string live at the very start of every real binary.
    let mut head = vec![0u8; 65536];
    let n = unsafe { pread(prog_fd, head.as_mut_ptr() as *mut c_void, head.len(), 0) };
    if n < 0 {
        die(&format!("read {}", resolved.display()), "");
    }
    head.truncate(n as usize);
    let kind = match elf_interp(&head) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("outrig-enter: {}: {e}", resolved.display());
            std::process::exit(1);
        }
    };

    // Everything the exec will later name is settled here, against this image's
    // rootfs; see the ordering contract.
    let dynamic = match &kind {
        ElfKind::Static => None,
        ElfKind::Dynamic(interp) => {
            let program = canonical_or_die("PROGRAM", resolved.as_os_str().as_bytes());
            let loader = canonical_or_die("interpreter", interp.as_bytes());
            // Confirmed present here rather than left to the exec: after the
            // setns a loader path that reaches one of the primary's files is a
            // segfault with nothing on stderr, and one that reaches none of
            // them is an error naming a path the user never wrote.
            if !exists(&loader) {
                die(
                    &format!("open interpreter {loader}"),
                    " (the payload's PT_INTERP; not in the sidecar image)",
                );
            }
            Some(DynamicLaunch {
                program,
                loader,
                lib_dirs: present_lib_dirs(),
                // Read from the interpreter as the image wrote it, not as it
                // resolves: an image is free to route `ld-musl-*.so.1` through
                // a differently named real file, and which loader this is does
                // not change when its path does.
                musl: interp.contains("ld-musl"),
            })
        }
    };

    let root = cstr(b"/");
    // A static payload resolves nothing through either rootfs, so it needs no
    // graft -- only the dynamic path snapshots the sidecar's root before setns.
    let tree_fd = if dynamic.is_some() {
        let fd = unsafe {
            syscall(SYS_OPEN_TREE, AT_FDCWD, root.as_ptr(), OPEN_TREE_CLONE | AT_RECURSIVE)
        } as c_int;
        if fd < 0 {
            die("open_tree(/)", " (missing CAP_SYS_ADMIN in this user namespace?)");
        }
        Some(fd)
    } else {
        None
    };

    let ns_path: OsString = ns_file.unwrap_or_else(|| OsString::from(format!("/proc/{target}/ns/mnt")));
    let ns_c = cstr(ns_path.as_bytes());
    let ns_fd = unsafe { open(ns_c.as_ptr(), O_RDONLY) };
    if ns_fd < 0 {
        let hint = if std::io::Error::last_os_error().raw_os_error() == Some(EACCES) {
            " (missing --cap-add=SYS_PTRACE?)"
        } else {
            ""
        };
        die(&format!("open {}", ns_path.to_string_lossy()), hint);
    }
    if unsafe { setns(ns_fd, CLONE_NEWNS) } < 0 {
        let hint = if std::io::Error::last_os_error().raw_os_error() == Some(EPERM) {
            " (need --cap-add=SYS_ADMIN and --userns=container:<target>)"
        } else {
            ""
        };
        die("setns(CLONE_NEWNS)", hint);
    }
    unsafe { close(ns_fd) };
    // From here the sidecar's own filesystem is gone, reachable only via the fds
    // opened above.

    // Private copy first, so neither the graft nor the `/proc` below is visible
    // to the target container. Unconditional: a static payload needs no graft
    // but still gets its own `/proc`, and mounting that in the target's own
    // namespace would replace the primary's.
    if unsafe { unshare(CLONE_NEWNS) } < 0 {
        die("unshare(CLONE_NEWNS)", "");
    }
    if unsafe {
        mount(std::ptr::null(), root.as_ptr(), std::ptr::null(), MS_REC | MS_SLAVE, std::ptr::null())
    } < 0
    {
        die("mount(MS_REC|MS_SLAVE)", "");
    }

    if let Some(tree_fd) = tree_fd {
        let graft_c = cstr(graft.as_bytes());
        let empty = cstr(b"");
        let r = unsafe {
            syscall(
                SYS_MOVE_MOUNT,
                tree_fd as c_long,
                empty.as_ptr(),
                AT_FDCWD,
                graft_c.as_ptr(),
                MOVE_MOUNT_F_EMPTY_PATH,
            )
        };
        if r < 0 {
            let g = graft.to_string_lossy();
            die(&format!("move_mount -> {g}"), &format!(" (does {g} exist in the target image?)"));
        }
        unsafe { close(tree_fd) };
    }

    // The `/proc` we inherited is the target's: an instance of *its* PID
    // namespace, which has no entry for this process because `setns` joined the
    // mount namespace only. `/proc/self` there resolves to nothing, and a
    // payload that reads it fails with something unrelated to what it asked for
    // -- rustup's `cargo` shim reports "no /proc/self/exe available. Is /proc
    // mounted?". A fresh mount is this process's own namespace instead. The
    // target's process list goes out of view with it, which is right: this is a
    // filesystem view.
    let proc_fs = cstr(b"proc");
    let proc_dst = cstr(b"/proc");
    if unsafe {
        mount(
            proc_fs.as_ptr(),
            proc_dst.as_ptr(),
            proc_fs.as_ptr(),
            MS_NOSUID | MS_NODEV | MS_NOEXEC,
            std::ptr::null(),
        )
    } < 0
    {
        die("mount(/proc)", " (does /proc exist in the target image?)");
    }

    // glibc's loader opens `/etc/ld.so.preload` by that absolute name on every
    // start and loads each object it lists before `main`; no loader option turns
    // that off. After the join the file is the primary's, so the libraries the
    // primary preloads would be loaded into this payload (#268). The loader
    // skips a zero-length file silently, so bind `/dev/null` over it -- in the
    // private namespace made above, which leaves the primary its real file.
    // `absent` and `mount` both follow a symlink at the path as the loader's
    // `open` does, and a file the primary bind-mounted there is stacked over. No
    // file is the common case and needs nothing, and only the kernel's "not
    // there" answers count as no file -- an unreadable one is still the loader's
    // to read (#600). One that cannot be masked is fatal, because starting the
    // payload unmasked is what this step exists to prevent. There is no
    // read-only remount: in a user namespace it has to restate every flag the
    // source mount carries, and `/dev/null` discards writes anyway. musl's
    // loader never reads the file, and a static payload has no loader.
    if dynamic.as_ref().is_some_and(|d| !d.musl) && !absent("/etc/ld.so.preload") {
        let dev_null = cstr(b"/dev/null");
        let preload = cstr(b"/etc/ld.so.preload");
        if unsafe {
            mount(
                dev_null.as_ptr(),
                preload.as_ptr(),
                std::ptr::null(),
                MS_BIND,
                std::ptr::null(),
            )
        } < 0
        {
            die(
                "mount(/dev/null -> /etc/ld.so.preload)",
                " (hiding the primary's preload file from the payload's loader)",
            );
        }
    }

    let cwd_c = cstr(cwd.as_bytes());
    if unsafe { chdir(cwd_c.as_ptr()) } < 0 {
        die(&format!("chdir {}", cwd.to_string_lossy()), "");
    }

    // Last privileged instant; see the ordering contract in the module docs.
    if let Some((uid, gid)) = drop_to {
        drop_privileges(uid, gid);
    }

    match dynamic {
        None => {
            // Static: run straight from the fd, so nothing resolves through the
            // target and its libc is irrelevant. `argv[0]` stays as the caller
            // wrote it -- a name found on `PATH` reaches the payload as that
            // name, which is what `execvp` hands it too.
            let argv: Vec<CString> = prog_argv.iter().map(|a| cstr(a.as_bytes())).collect();
            let envp = environ_cstrings();
            let argv_p = arg_ptrs(&argv);
            let envp_p = arg_ptrs(&envp);
            let empty = cstr(b"");
            unsafe {
                syscall(
                    SYS_EXECVEAT,
                    prog_fd as c_long,
                    empty.as_ptr(),
                    argv_p.as_ptr(),
                    envp_p.as_ptr(),
                    AT_EMPTY_PATH,
                );
            }
            die("execveat", "");
        }
        Some(dynamic) => {
            // Run through the sidecar's own loader, now under the graft point,
            // at the paths resolved before the setns. Every one of these is
            // symlink-free already, so prefixing the graft is the last thing
            // that happens to them. This is also why the payload's `exe` link is
            // the loader and its `argv[0]` is `progpath` -- see #358 in the
            // module docs.
            let g = graft.to_string_lossy();
            let loader = format!("{g}{}", dynamic.loader);
            let progpath = format!("{g}{}", dynamic.program);
            let libpath = dynamic
                .lib_dirs
                .iter()
                .map(|d| format!("{g}{d}"))
                .collect::<Vec<_>>()
                .join(":");

            let mut launch: Vec<CString> = Vec::new();
            launch.push(cstr(loader.as_bytes()));
            // musl's loader takes --library-path but not glibc's --inhibit-cache,
            // and MCP images are very often Alpine-based.
            if !dynamic.musl {
                launch.push(cstr(b"--inhibit-cache"));
            }
            launch.push(cstr(b"--library-path"));
            launch.push(cstr(libpath.as_bytes()));
            launch.push(cstr(progpath.as_bytes()));
            for a in &prog_argv[1..] {
                launch.push(cstr(a.as_bytes()));
            }
            // launch[0] is the loader CString; reuse its pointer via launch_p.
            let launch_p = arg_ptrs(&launch);
            unsafe { execv(launch_p[0], launch_p.as_ptr()) };
            die(&format!("execv {loader}"), "");
        }
    }
}
