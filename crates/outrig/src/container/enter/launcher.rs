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
//! Expected launch (0090 arranges it): `--userns=container:<target>` (so we are
//! already in the user namespace that owns the target's mount namespace),
//! `--cap-add=SYS_ADMIN` (setns is gated on it) and `--cap-add=SYS_PTRACE`
//! (to open the target's nsfs file).
//!
//! **The argv contract, which the caller depends on:** `PROGRAM` is in *this*
//! container's coordinates, ungrafted -- it is opened before the setns, while
//! this image's rootfs is still at `/`, and the graft is applied to it here
//! when handing the path to the loader. A `PROGRAM` with no `/` in it is
//! searched along this process's own `PATH` first (see `path_search.rs`), which
//! is the sidecar image's -- the same environment the image's `ENTRYPOINT`
//! would have resolved against had podman run it directly. `ARGS...` are in the
//! *target's* coordinates and are passed through untouched.
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
//!
//! This file is compiled only by the `outrig` crate's `build.rs`, always for a
//! Linux musl target; it is not part of the normal `cargo build`. The pure
//! logic it relies on lives in `elf.rs` and `path_search.rs`, pulled in below
//! and unit-tested on the host.

use std::ffi::{CString, OsString, c_char, c_int, c_long, c_ulong, c_void};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

include!("elf.rs");
include!("path_search.rs");

unsafe extern "C" {
    fn open(path: *const c_char, flags: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
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
const AT_FDCWD: c_long = -100;
const AT_EMPTY_PATH: c_long = 0x1000;
const AT_RECURSIVE: c_long = 0x8000;
const OPEN_TREE_CLONE: c_long = 1;
const MOVE_MOUNT_F_EMPTY_PATH: c_long = 0x04;
const CLONE_NEWNS: c_int = 0x0002_0000;
const MS_NOSUID: c_ulong = 2;
const MS_NODEV: c_ulong = 4;
const MS_NOEXEC: c_ulong = 8;
const MS_REC: c_ulong = 0x4000;
const MS_SLAVE: c_ulong = 1 << 19;
const EPERM: i32 = 1;
const EACCES: i32 = 13;

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

    let root = cstr(b"/");
    // A static payload resolves nothing through either rootfs, so it needs no
    // graft -- only the dynamic path snapshots the sidecar's root before setns.
    let tree_fd = if matches!(kind, ElfKind::Dynamic(_)) {
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

    let cwd_c = cstr(cwd.as_bytes());
    if unsafe { chdir(cwd_c.as_ptr()) } < 0 {
        die(&format!("chdir {}", cwd.to_string_lossy()), "");
    }

    // Last privileged instant; see the ordering contract in the module docs.
    if let Some((uid, gid)) = drop_to {
        drop_privileges(uid, gid);
    }

    match kind {
        ElfKind::Static => {
            // Run straight from the fd: nothing resolves through the target, so
            // its libc is irrelevant. `argv[0]` stays as the caller wrote it --
            // a name found on `PATH` reaches the payload as that name, which is
            // what `execvp` hands it too.
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
        ElfKind::Dynamic(interp) => {
            // Run through the sidecar's own loader, now under the graft
            // point, at the path the lookup settled on.
            let g = graft.to_string_lossy();
            let loader = format!("{g}{interp}");
            let progpath = format!("{g}{}", resolved.display());
            let libpath = lib_dirs()
                .iter()
                .map(|d| format!("{g}{d}"))
                .collect::<Vec<_>>()
                .join(":");

            let mut launch: Vec<CString> = Vec::new();
            launch.push(cstr(loader.as_bytes()));
            // musl's loader takes --library-path but not glibc's --inhibit-cache,
            // and MCP images are very often Alpine-based.
            if !interp.contains("ld-musl") {
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
