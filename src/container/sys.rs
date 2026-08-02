//! The sandbox child syscall pipeline (`ri-sandbox`).
//!
//! Runs single-threaded, before any async runtime, so `unshare(CLONE_NEWUSER)`
//! is legal. Sequence (mirrored from `docs/CONTAINER-RUNTIME-SPEC.md` §4):
//!
//! 1. `unshare(CLONE_NEWUSER | CLONE_NEWNS)` — requires no privileges; inside
//!    the fresh user namespace the process owns uid 0 and `CAP_SYS_ADMIN` over
//!    its own mount namespace.
//! 2. Write `setgroups=deny`, then `gid_map`/`uid_map` mapping the invoking
//!    uid/gid → 0 (the creating process may map itself — `unshare -Ur` does
//!    exactly this).
//! 3. Set up the scrubbed view inside the image via `mount(2)`:
//!    - `proc` at `/proc` (best-effort),
//!    - host loader/lib dirs (`/lib`, `/lib64`, `/usr/lib*`, and the
//!      NixOS-specific `/nix/store`, `/run/current-system/sw`) read-only for
//!      the copied shell,
//!    - non-secret `/etc/*` files,
//!    - `/tmp` (shared scratch), `/work` (bind of the tool cwd, writable),
//!      `/tools` + `/tools-local` (custom tools, writable),
//!    - when no static coreutils is installed: host `/bin`, `/sbin`, `/usr`
//!      read-only fallback,
//!    - finally, remount the image root read-only,
//! 4. `chroot` into the image, `chdir("/work")`, set a sandboxed `PATH`/`HOME`,
//!    and `execvp` the requested program.
//!
//! Network namespace is intentionally *not* isolated (host network shared).

// This module is compiled into TWO crate targets: the `ri-sandbox` bin (where
// `run_child` is the entry point) and the `ri` bin (where it is never called —
// subprocess.rs only uses `container::sandbox_argv`). The compiler cannot see
// the cross-target use, so the `ri` build sees `run_child` and its helpers as
// dead. They are not: remove this only if `run_child` gains a real caller.
#![allow(dead_code)]

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};

const MS_BIND: libc::c_ulong = libc::MS_BIND;
const MS_RDONLY: libc::c_ulong = libc::MS_RDONLY;
const MS_REMOUNT: libc::c_ulong = libc::MS_REMOUNT;
const MS_NOSUID: libc::c_ulong = libc::MS_NOSUID;
const MS_NODEV: libc::c_ulong = libc::MS_NODEV;
const MS_NOEXEC: libc::c_ulong = libc::MS_NOEXEC;

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::other("path contains a NUL byte"))
}

/// Bind `src` over `dst` (both host-visible at call time).
fn bind_mount(src: &str, dst: &Path) -> io::Result<()> {
    let s = cstr(src)?;
    let d = cstr(&dst.to_string_lossy())?;
    // SAFETY: `mount` only reads the CString pointers we keep alive for the
    // call; flags select a bind mount with the system source `""`.
    let rc = unsafe {
        libc::mount(
            s.as_ptr(),
            d.as_ptr(),
            std::ptr::null(),
            MS_BIND,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Remount a path with `flags` added (used to flip the image root to RO).
fn remount_flags(dst: &Path, add: libc::c_ulong) -> io::Result<()> {
    let d = cstr(&dst.to_string_lossy())?;
    // SAFETY: bind-remount of an existing mount point; read-only hardening.
    let rc = unsafe {
        libc::mount(
            d.as_ptr(),
            d.as_ptr(),
            std::ptr::null(),
            MS_BIND | MS_REMOUNT | add,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Run the sandbox: never returns on success (the requested program replaces
/// this process via `execvp`).
pub fn run_child(image: &Path, argv: &[String], binds: &[Binds]) -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // ── 1. user + mount namespaces (must precede cred/mount work) ──────────
    // SAFETY: single-threaded child (ri-sandbox main runs pre-tokio). `unshare`
    // with CLONE_NEWUSER|CLONE_NEWNS creates the two namespaces; it requires no
    // privilege and fails with EINVAL only if we were multi-threaded.
    let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) };
    if rc != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "unshare(CLONE_NEWUSER|CLONE_NEWNS) failed: {}",
                io::Error::last_os_error()
            ),
        ));
    }

    // ── 2. map our uid/gid → 0 inside the new user namespace ───────────────
    // SAFETY rules for /proc/self/*: no error return on these write()s; any
    // failure is reported via the map-file contents below.
    let _ = std::fs::write("/proc/self/setgroups", "deny\n");
    let _ = std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"));
    let _ = std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"));

    // ── 3. scrub: bind host dirs/files read-only into the image ────────────
    // self-bind the image root so it can be flipped read-only last.
    match bind_mount(&image.to_string_lossy(), image) {
        Ok(()) => {}
        Err(e) => return Err(e),
    }

    // Loader/lib dirs for the copied dynamic shell + any other dynamic tool.
    for d in [
        "/lib",
        "/lib64",
        "/lib32",
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib32",
        "/nix/store",
        "/run/current-system/sw",
    ] {
        if Path::new(d).is_dir() && d != "/" {
            let target = image.join(d.trim_start_matches('/'));
            if target.is_dir() {
                let _ = bind_mount(d, &target); // best-effort; EPERM → skip
            }
        }
    }

    // Non-secret /etc files (DNS + user/group resolution) as single-file binds.
    for f in ["passwd", "group", "resolv.conf", "nsswitch.conf", "hosts"] {
        let src = Path::new("/etc").join(f);
        let dst = image.join("etc").join(f);
        if src.is_file() && dst.is_file() {
            let _ = bind_mount(&src.to_string_lossy(), &dst);
        }
    }

    enum Marker {
        One,
        Zero,
        Missing,
    }
    let marker = |name: &str| -> Marker {
        match std::fs::read_to_string(image.join(name)) {
            Ok(s) if s.trim() == "1" => Marker::One,
            Ok(_) => Marker::Zero,
            Err(_) => Marker::Missing,
        }
    };
    let has_uutils = matches!(marker("has-uutils"), Marker::One);
    // Strict image: static ri-sh shell + static coreutils → the image is fully
    // self-contained, so no host /lib loader dirs and no /bin,/usr fallback
    // are mounted at all. Any other combination falls back to host binds.
    let strict = has_uutils && matches!(marker("has-static-sh"), Marker::One);

    // Loader/lib dirs are only needed when the shell or fallback tools are
    // dynamic (i.e. the image is not fully static).
    if !strict {
        for d in [
            "/nix/store",
            "/run/current-system/sw",
            "/lib",
            "/lib64",
            "/lib32",
            "/usr/lib",
            "/usr/lib64",
            "/usr/lib32",
        ] {
            if Path::new(d).is_dir() {
                let target = image.join(d.trim_start_matches('/'));
                if target.is_dir() {
                    let _ = bind_mount(d, &target);
                }
            }
        }
    }

    // Fallback: without static coreutils, expose host /bin,/sbin,/usr RO so
    // common file tools exist. Skipped when the image ships its own tools.
    if !has_uutils {
        for d in ["/bin", "/sbin", "/usr"] {
            if Path::new(d).is_dir() {
                let target = image.join(d.trim_start_matches('/'));
                if target.is_dir() {
                    let _ = bind_mount(d, &target);
                }
            }
        }
    }

    // ── self-owned Rust toolchain (spec §6/§16) ─────────────────────────────
    // The toolchain is bound read-only at /toolchain plus its two runtime libs
    // (musl loader + gcc runtime) are bound into the image so the musl-host
    // `rustc` runs inside the chroot. The toolchain is never copied into the
    // image (hundreds of MB) — it is the app's own downloaded bundle.
    let tc_dir = match std::fs::read_to_string(image.join("toolchain-dir")) {
        Ok(d) if !d.trim().is_empty() => PathBuf::from(d.trim()),
        _ => std::path::PathBuf::new(),
    };
    if !tc_dir.as_os_str().is_empty() && tc_dir.is_dir() {
        let tc_mount = image.join("toolchain");
        if tc_mount.is_dir() {
            let _ = bind_mount(&tc_dir.to_string_lossy(), &tc_mount);
        }
        let loader = tc_dir.join("lib").join("ld-musl-x86_64.so.1");
        let loader_dst = image.join("lib").join("ld-musl-x86_64.so.1");
        if loader.is_file() && loader_dst.is_file() {
            let _ = bind_mount(&loader.to_string_lossy(), &loader_dst);
        }
        let gcc = tc_dir.join("lib").join("libgcc_s.so.1");
        let gcc_dst = image.join("usr/lib").join("libgcc_s.so.1");
        if gcc.is_file() && gcc_dst.is_file() {
            let _ = bind_mount(&gcc.to_string_lossy(), &gcc_dst);
        }
        // musl C cross compiler (musl.cc `x86_64-linux-musl-cross`, bundled at
        // `<tc>/musl-cross`). Its driver is a fully static i386 binary and it
        // is relocatable (empty `--prefix`; sysroot resolved relative to the
        // gcc location), so binding it at the canonical install prefix is all
        // that is needed — no host musl-dev, no extra libs.
        let xcc = tc_dir.join("musl-cross");
        let xcc_mount = image.join("x86_64-linux-musl-cross");
        if xcc.is_dir() && xcc_mount.is_dir() {
            let _ = bind_mount(&xcc.to_string_lossy(), &xcc_mount);
        }
    }

    // ── device nodes (best-effort single-file binds over placeholders) ─────
    for dev in ["null", "zero", "random", "urandom", "tty", "full"] {
        let src = Path::new("/dev").join(dev);
        let dst = image.join("dev").join(dev);
        if src.exists() && dst.exists() {
            let _ = bind_mount(&src.to_string_lossy(), &dst);
        }
    }

    // ── proc for tools that need it (best-effort) ──────────────────────────
    let proc_dir = image.join("proc");
    if proc_dir.is_dir() {
        let p = cstr(&proc_dir.to_string_lossy())?;
        // SAFETY: mounting proc inside the user+pid context of this namespace;
        // permitted for an unprivileged userns owner with CAP_SYS_ADMIN over
        // its own mount namespace. Best-effort: failure only affects tools
        // that read /proc.
        let rc = unsafe {
            libc::mount(
                c"proc".as_ptr(),
                p.as_ptr(),
                c"proc".as_ptr(),
                MS_NOSUID | MS_NODEV | MS_NOEXEC,
                std::ptr::null(),
            )
        };
        let _ = rc; // best-effort: on hosts without procfs, tools fall back
    }

    // ── writable scratch: /tmp shared with the host ────────────────────────
    let tmp_img = image.join("tmp");
    if tmp_img.is_dir() {
        let _ = bind_mount("/tmp", &tmp_img);
    }

    // ── the tool cwd is bound to /work (writable) ──────────────────────────
    let work_img = image.join("work");
    if work_img.is_dir() {
        let _ = bind_mount(&cwd.to_string_lossy(), &work_img);
    }

    // ── explicit writable binds passed by the caller: /tools, ... ──────────
    for b in binds {
        if Path::new(&b.host).is_dir() || Path::new(&b.host).is_file() {
            let guest = if b.guest.starts_with('/') {
                &b.guest[1..]
            } else {
                &b.guest
            };
            let target = image.join(guest);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if !target.exists() {
                if Path::new(&b.host).is_dir() {
                    let _ = std::fs::create_dir_all(&target);
                } else {
                    let _ = std::fs::write(&target, b"");
                }
            }
            let _ = bind_mount(&b.host, &target);
        }
    }

    // ── harden the image root read-only (writes confined to the binds) ─────
    let _ = remount_flags(image, MS_RDONLY);

    // ── 4. chroot + exec ───────────────────────────────────────────────────
    let root = cstr(&image.to_string_lossy())?;
    // SAFETY: `chroot` into the assembled image; the caller constructed it.
    if unsafe { libc::chroot(root.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: move to the work bind (the caller's cwd) after the chroot.
    if unsafe { libc::chdir(c"/work".as_ptr()) } != 0 {
        // Fall back to the image root when /work is unavailable.
        if unsafe { libc::chdir(c"/".as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    // A clean, sandboxed environment for the requested program. `set_var` is
    // unsafe in edition 2024 (can race reads); the child is single-threaded
    // here and execs immediately after, so there is no concurrent access.
    unsafe {
        std::env::set_var("PATH", "/bin:/usr/bin:/sbin:/usr/sbin:/tools:/tools-local");
        std::env::set_var("HOME", "/home/ri");
        std::env::set_var("TERM", "dumb");
        std::env::set_var("NO_COLOR", "1");
        // Container-shaped hints some tools grep for.
        std::env::set_var("container", "ri-sandbox");
        // The musl-host rustc has no \$ORIGIN support in RPATH, so its libs
        // are found via LD_LIBRARY_PATH (toolchain/lib + the loader dirs).
        std::env::set_var("LD_LIBRARY_PATH", "/toolchain/lib:/usr/lib:/lib");
    }

    // ── resource limits (spec §7): constrain runaway tools ─────────────────
    // Hard limits so a tool cannot raise them back. Configurable via
    // `$RI_SANDBOX_RLIMITS` (`name=value,…`); `none` disables. Units: bytes
    // with optional k/m/g suffix; cpu in seconds; nproc/nofile as a count.
    let limits = std::env::var("RI_SANDBOX_RLIMITS")
        .unwrap_or_else(|_| "cpu=30,nproc=64,nofile=2048,as=512m,fsize=1g,core=0".to_string());
    if limits != "none" {
        apply_rlimits(&limits);
    }

    // ── seccomp denylist (spec §7): block namespace/fs/kernel escapes ─────
    // Default-action ALLOW keeps tools (and cargo / network stacks) working;
    // a curated set of syscalls that could leak into or mutate the host is
    // instead answered with EPERM. Installed after RLIMIT, before exec, in
    // this single-threaded child.
    if std::env::var("RI_SANDBOX_NO_SECCOMP")
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
    {
        eprintln!("sandbox: seccomp disabled by RI_SANDBOX_NO_SECCOMP");
    } else {
        match install_seccomp_denylist() {
            Ok(()) => {}
            Err(e) => eprintln!("sandbox: seccomp install failed, continuing: {e}"),
        }
    }

    if argv.is_empty() {
        return Err(io::Error::other("sandbox: empty command"));
    }

    // Build the argv array (NULL-terminated) and execvp. On success execvp
    // does not return; a failure is reported so the parent sees stderr + exit.
    let mut cargs: Vec<CString> = Vec::with_capacity(argv.len());
    for a in argv {
        cargs.push(cstr(a)?);
    }
    let mut ptrs: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    // SAFETY: `cargs` outlives the call; `execvp` replaces this process with
    // the requested program using the (still-valid) pointer array. If we reach
    // the line after it, execvp failed and the error is reported so the parent
    // sees a useful stderr + non-zero exit.
    unsafe { libc::execvp(cargs[0].as_ptr(), ptrs.as_ptr()) };
    Err(io::Error::other(format!(
        "execvp({}) failed: {}",
        argv[0],
        io::Error::last_os_error()
    )))
}

/// Parse a byte-size value with an optional `k`/`m`/`g` suffix.
fn parse_size(s: &str) -> Option<libc::rlim_t> {
    let (num, mult) = match s.as_bytes().last().copied() {
        Some(b'k') | Some(b'K') => (&s[..s.len() - 1], 1024u64),
        Some(b'm') | Some(b'M') => (&s[..s.len() - 1], 1024u64 * 1024),
        Some(b'g') | Some(b'G') => (&s[..s.len() - 1], 1024u64 * 1024 * 1024),
        _ => (s, 1u64),
    };
    num.parse::<u64>().ok().map(|n| n.saturating_mul(mult))
}

/// Set a single `name=value` resource limit (best-effort).
fn apply_one(name: &str, value: &str) {
    let (res, lim): (libc::c_int, Option<libc::rlim_t>) = match name {
        "cpu" => (
            libc::RLIMIT_CPU,
            value.parse::<u64>().ok().map(|n| n.max(1)),
        ),
        "nproc" => (libc::RLIMIT_NPROC, value.parse::<u64>().ok()),
        "nofile" => (libc::RLIMIT_NOFILE, value.parse::<u64>().ok()),
        "as" => (libc::RLIMIT_AS, parse_size(value)),
        "fsize" => (libc::RLIMIT_FSIZE, parse_size(value)),
        "core" => (libc::RLIMIT_CORE, parse_size(value)),
        "stack" => (libc::RLIMIT_STACK, parse_size(value)),
        _ => return,
    };
    let Some(lim) = lim else { return };
    let rlim = libc::rlimit {
        rlim_cur: lim,
        rlim_max: lim,
    };
    // SAFETY: setrlimit only writes the rlimit we pass; the child is
    // single-threaded (numeric arguments; no Rust invariants involved).
    unsafe { libc::setrlimit(res, &rlim) };
}

/// Apply the comma-separated `name=value` limit list.
fn apply_rlimits(spec: &str) {
    for pair in spec.split(',').filter(|p| !p.is_empty()) {
        if let Some((name, value)) = pair.split_once('=') {
            apply_one(name.trim(), value.trim());
        }
    }
}

// ── seccomp (spec §7): raw-BPF denylist ──────────────────────────────────────
//
// The shipped BPF answers `SECCOMP_RET_ERRNO(EPERM)` for a curated list of
// syscalls that could leak into or mutate the host, and lets everything else
// through (denylist, not allowlist — cargo, network stacks and the varied
// tool behaviours keep working). Written by hand as raw `sock_filter`s so the
// musl-static `ri-sandbox` binary needs no extra dependency.
//
// Blocked classes:
// - mount-namespace / chroot escapes: mount, umount*, pivot_root, chroot,
//   open_by_handle_at / name_to_handle_at (file-handle walkouts), unshare,
//   setns, mknod / mknodat (device node creation),
// - SysV shared memory: shmget/shmctl/shmat/shmdt (host-VISIBLE because the
//   sandbox does not create its own IPC namespace),
// - kernel interface abuse: init/finit/delete_module, reboot, kexec*,
//   bpf, perf_event_open, userfaultfd, io_uring_*, fanotify_*,
// - cross-process/trace: ptrace, process_vm_readv/writev, kcmp,
// - kernel keyring (host side effects): keyctl, add_key, request_key,
// - host-visible hostname/privilege knobs: sethostname, setdomainname,
//   ioperm, iopl, reboot, syslog, acct, quotactl, swapon/swapoff, modify_ldt,
// - raw packet sockets: socket(AF_PACKET) via an argument filter on arg[0].
//
// Since the child is single-threaded here, the filter is installed before any
// threads exist and therefore applies to every future thread and to exec'd
// programs. `$RI_SANDBOX_NO_SECCOMP=1` skips installation (debug escape).

/// x86_64 syscall numbers used by the denylist. Kept as literals instead of
/// `libc::SYS_*` because several (io_uring, open_by_handle_at) are not
/// defined for the musl target in the `libc` crate.
///
/// A second, I386 list exists because the bundled musl.cc `x86_64-linux-musl-
/// cross` C compiler is a *32-bit* static driver (i386), so the filter must
/// boot both architectures (`unshare(CLONE_NEWUSER)` on x86_64 hosts runs
/// legacy i386 binaries; killing them would break the muslcc tool). Syscall
/// numbers differ per arch, hence two chains. On i386 SysV IPC goes through
/// the `ipc(2)` multiplexer (117) and raw sockets through `socketcall(2)`
/// — neither is argument-filterable here, so those two gaps are accepted.
const DENY_SYSCALLS_X86_64: &[u64] = &[
    165, // mount
    23,  // umount (legacy)
    166, // umount2
    155, // pivot_root
    161, // chroot
    272, // unshare
    308, // setns
    304, // open_by_handle_at
    303, // name_to_handle_at
    133, // mknod
    259, // mknodat
    29,  // shmget
    30,  // shmctl
    31,  // shmat
    32,  // shmdt
    175, // init_module
    176, // delete_module
    313, // finit_module
    169, // reboot
    246, // kexec_load
    320, // kexec_file_load
    321, // bpf
    298, // perf_event_open
    323, // userfaultfd
    425, // io_uring_setup
    426, // io_uring_enter
    427, // io_uring_register
    300, // fanotify_init
    301, // fanotify_mark
    101, // ptrace
    310, // process_vm_readv
    311, // process_vm_writev
    312, // kcmp
    250, // keyctl
    248, // add_key
    249, // request_key
    170, // sethostname
    171, // setdomainname
    172, // ioperm
    173, // iopl
    103, // syslog
    163, // acct
    179, // quotactl
    167, // swapon
    168, // swapoff
    154, // modify_ldt
];

/// `socket` syscall id (x86_64) and the AF_PACKET domain value used to reject
/// raw packet sockets without blocking normal networking.
const SYS_SOCKET_X86_64: u64 = 41;
const AF_PACKET: u64 = 17;

/// I386 (32-bit) syscall numbers for the same deny classes. Differs from the
/// x86_64 table (IPC multiplexed, no native socket subcalls to filter).
#[rustfmt::skip]
const DENY_SYSCALLS_I386: &[u64] = &[
    21,   // mount
    22,   // umount (legacy)
    52,   // umount2
    217,  // pivot_root
    61,   // chroot
    310,  // unshare
    346,  // setns
    342,  // open_by_handle_at
    341,  // name_to_handle_at
    14,   // mknod
    397,  // mknodat
    117,  // ipc (SysV shm/sem/msg multiplexer)
    128,  // init_module
    129,  // delete_module
    379,  // finit_module
    88,   // reboot
    283,  // kexec_load
    372,  // kexec_file_load
    357,  // bpf
    336,  // perf_event_open
    374,  // userfaultfd
    425,  // io_uring_setup
    426,  // io_uring_enter
    427,  // io_uring_register
    358,  // fanotify_init
    359,  // fanotify_mark
    26,   // ptrace
    347,  // process_vm_readv
    348,  // process_vm_writev
    349,  // kcmp
    288,  // keyctl
    286,  // add_key
    287,  // request_key
    74,   // sethostname
    75,   // setdomainname
    101,  // ioperm
    110,  // iopl
    103,  // syslog
    51,   // acct
    131,  // quotactl
    87,   // swapon
    115,  // swapoff
    123,  // modify_ldt
];

// BPF instruction constants (linux/filter.h).
const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_RET: u16 = 0x06;
const BPF_K: u16 = 0x00;

// struct seccomp_data field offsets.
const SECCOMP_NR_OFF: u32 = 0;
const SECCOMP_ARCH_OFF: u32 = 4;
const SECCOMP_ARG0_OFF: u32 = 16;

// AUDIT_ARCH_* = EM_* | __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE.
//   X86_64 = 62 | 0x80000000 | 0x40000000 ; I386 = 3 | 0x40000000 (32-bit LE).
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const AUDIT_ARCH_I386: u32 = 0x4000_0003;

// seccomp return actions.
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO_EPERM: u32 = 0x0005_0001;
const RET_KILL: u32 = 0x8000_0000;

fn inst(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

/// Build one architecture's deny chain: `[LD nr]` + one `JEQ` per denied
/// syscall (matching jumps to the EPERM terminal), an optional raw-socket
/// sub-branch, the ALLOW terminal, then the shared EPERM deny terminal.
/// Everything is self-contained — only internal (8-bit) relative jumps.
fn build_arch_chain(deny: &[u64], socket_nr: Option<u64>) -> Vec<libc::sock_filter> {
    let mut c: Vec<libc::sock_filter> = Vec::with_capacity(4 + deny.len());
    c.push(inst(BPF_LD | BPF_W | BPF_ABS, SECCOMP_NR_OFF)); // 0
    let chain_start = c.len(); // 1
    for nr in deny {
        c.push(inst(BPF_JMP | BPF_JEQ, (*nr) as u32));
    }

    if let Some(sn) = socket_nr {
        let s = c.len();
        c.push(inst(BPF_LD | BPF_W | BPF_ABS, SECCOMP_NR_OFF)); // S
        c.push(inst(BPF_JMP | BPF_JEQ, sn as u32)); // S+1
        c.push(inst(BPF_RET | BPF_K, RET_ALLOW)); // S+2 (not a socket call)
        c.push(inst(BPF_LD | BPF_W | BPF_ABS, SECCOMP_ARG0_OFF)); // S+3
        c.push(inst(BPF_JMP | BPF_JEQ, AF_PACKET as u32)); // S+4
        c.push(inst(BPF_RET | BPF_K, RET_ALLOW)); // S+5 (socket, ok family)

        let d = c.len(); // the deny terminal
        c.push(inst(BPF_RET | BPF_K, RET_ERRNO_EPERM));

        // Patch the deny JEQs and the two socket-branch JEQs to `d`.
        for (i, _) in deny.iter().enumerate() {
            let idx = chain_start + i;
            let target = d - idx - 1;
            assert!(target < 256, "seccomp deny chain too long");
            c[idx].jt = target as u8;
        }
        c[s + 1].jt = 1; // socket match → [S+3]
        let target = d - (s + 4) - 1;
        assert!(target < 256, "seccomp AF_PACKET branch too long");
        c[s + 4].jt = target as u8;
    } else {
        let allow = c.len();
        c.push(inst(BPF_RET | BPF_K, RET_ALLOW));
        let d = c.len(); // deny terminal
        c.push(inst(BPF_RET | BPF_K, RET_ERRNO_EPERM));

        for (i, _) in deny.iter().enumerate() {
            let idx = chain_start + i;
            let target = d - idx - 1;
            assert!(target < 256, "seccomp deny chain too long");
            c[idx].jt = target as u8;
        }
        let _ = allow; // non-matching syscalls fall through to it
    }
    c
}

/// Build the dual-arch denylist program (layout below). Kept separate from
/// [`install_seccomp_denylist`] so tests can verify the BPF without installing
/// it (which needs a single-threaded process).
fn build_denylist_program() -> Vec<libc::sock_filter> {
    //   [0]  LD arch
    //   [1]  JEQ AUDIT_ARCH_X86_64   jt→x64_start   jf=0
    //   [2]  JEQ AUDIT_ARCH_I386     jt→i386_start  jf=0
    //   [3]  RET_KILL               (unknown arch)
    //   x64_start:  x86_64 chain (deny + AF_PACKET socket check)
    //   i386_start: i386  chain (deny; IPC/socketcall gaps documented above)
    let mut prog: Vec<libc::sock_filter> =
        Vec::with_capacity(4 + DENY_SYSCALLS_X86_64.len() + DENY_SYSCALLS_I386.len());
    prog.push(inst(BPF_LD | BPF_W | BPF_ABS, SECCOMP_ARCH_OFF)); // 0
    prog.push(inst(BPF_JMP | BPF_JEQ, AUDIT_ARCH_X86_64)); // 1
    prog.push(inst(BPF_JMP | BPF_JEQ, AUDIT_ARCH_I386)); // 2
    prog.push(inst(BPF_RET | BPF_K, RET_KILL)); // 3

    let x64_start = prog.len();
    let x64 = build_arch_chain(DENY_SYSCALLS_X86_64, Some(SYS_SOCKET_X86_64));
    let i386_start = x64_start + x64.len();
    let i386 = build_arch_chain(DENY_SYSCALLS_I386, None);
    prog.extend(x64);
    prog.extend(i386);

    // Patch the arch dispatch jumps (8-bit; total program stays well under
    // 256 instructions, so the offsets fit).
    let x64_jt = x64_start - 1 - 1;
    let i386_jt = i386_start - 2 - 1;
    assert!(
        x64_jt < 256 && i386_jt < 256,
        "seccomp arch dispatch too long"
    );
    prog[1].jt = x64_jt as u8;
    prog[2].jt = i386_jt as u8;
    prog
}

/// Install the denylist filter. Returns an error describing a
/// `seccomp(2)`/`prctl(2)` failure (caller logs and continues).
fn install_seccomp_denylist() -> io::Result<()> {
    let mut prog = build_denylist_program();

    // SAFETY: `prctl`/`seccomp` only set the no-new-privs flag and install the
    // just-built filter; the pointers we pass are derived from `prog` which
    // outlives the call. Single-threaded child (pre-tokio ri-sandbox), so the
    // TSYNC flag is unnecessary — the filter applies to the whole (single)
    // thread and is inherited by future threads and exec'd programs.
    let rc = unsafe {
        let pnn = libc::prctl(38 /* PR_SET_NO_NEW_PRIVS */, 1, 0, 0, 0);
        if pnn != 0 {
            return Err(io::Error::last_os_error());
        }
        let fprog = libc::sock_fprog {
            len: prog.len() as u16,
            filter: prog.as_mut_ptr(),
        };
        libc::syscall(
            317, /* SYS_seccomp */
            1,   /* SECCOMP_SET_MODE_FILTER */
            0, &fprog,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A host → guest bind request passed into the child.
#[derive(Debug, Clone)]
pub struct Binds {
    /// Host path (absolute; visible to the child before the chroot).
    pub host: String,
    /// Guest path *inside* the image (absolute, e.g. `/work`).
    pub guest: String,
}

/// Convenience constructor for an externally supplied bind.
pub fn bind(host: impl Into<String>, guest: impl Into<String>) -> Binds {
    Binds {
        host: host.into(),
        guest: guest.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_parse_roundtrip() {
        let b = bind("/home/x", "/work");
        assert_eq!(b.host, "/home/x");
        assert_eq!(b.guest, "/work");
    }

    #[test]
    fn denylist_program_layout_is_sound() {
        let prog = build_denylist_program();

        // Dispatch head: LD arch ; JEQ X86_64 ; JEQ I386 ; RET_KILL.
        assert_eq!(prog[0].code, BPF_LD | BPF_W | BPF_ABS);
        assert_eq!(prog[0].k, SECCOMP_ARCH_OFF);
        assert_eq!(prog[1].code, BPF_JMP | BPF_JEQ);
        assert_eq!(prog[1].k, AUDIT_ARCH_X86_64);
        assert_eq!(prog[2].code, BPF_JMP | BPF_JEQ);
        assert_eq!(prog[2].k, AUDIT_ARCH_I386);
        assert_eq!(prog[3].code, BPF_RET | BPF_K);
        assert_eq!(prog[3].k, RET_KILL);

        let x64_start = 4usize;
        let n64 = DENY_SYSCALLS_X86_64.len();
        let ni = DENY_SYSCALLS_I386.len();

        // x86_64 chain: [LD nr] + n64 denies + 6-insn socket sub-branch + EPERM.
        let d_x64 = x64_start + 1 + n64 + 6;
        for (i, nr) in DENY_SYSCALLS_X86_64.iter().enumerate() {
            let idx = x64_start + 1 + i;
            assert_eq!(prog[idx].code, BPF_JMP | BPF_JEQ, "x64 deny {i}");
            assert_eq!(prog[idx].k, *nr as u32, "x64 deny {i} syscall id");
            assert_eq!(
                idx + 1 + prog[idx].jt as usize,
                d_x64,
                "x64 deny {i} must terminate at the EPERM terminal"
            );
        }
        // Socket sub-branch: not-socket and allowed-family fall to ALLOW;
        // AF_PACKET terminates at the same EPERM terminal.
        let s = x64_start + 1 + n64;
        assert_eq!(prog[s].k, SECCOMP_NR_OFF);
        assert_eq!(prog[s + 1].k, SYS_SOCKET_X86_64 as u32);
        assert_eq!(prog[s + 1].jt, 1);
        assert_eq!(prog[s + 2].code, BPF_RET | BPF_K);
        assert_eq!(prog[s + 2].k, RET_ALLOW);
        assert_eq!(prog[s + 3].k, SECCOMP_ARG0_OFF);
        assert_eq!(prog[s + 4].k, AF_PACKET as u32);
        assert_eq!(
            s + 4 + 1 + prog[s + 4].jt as usize,
            d_x64,
            "AF_PACKET must terminate at the EPERM terminal"
        );
        assert_eq!(prog[s + 5].code, BPF_RET | BPF_K);
        assert_eq!(prog[s + 5].k, RET_ALLOW);
        assert_eq!(prog[d_x64].code, BPF_RET | BPF_K);
        assert_eq!(prog[d_x64].k, RET_ERRNO_EPERM);

        // i386 chain follows immediately after the x86_64 one.
        let i386_start = d_x64 + 1;
        assert_eq!(prog[1].jt as usize, x64_start - 2, "x86_64 dispatch offset");
        assert_eq!(prog[2].jt as usize, i386_start - 3, "i386 dispatch offset");
        // i386 chain: [LD nr] + ni denies + ALLOW + EPERM (no socket branch).
        let d_i386 = i386_start + 2 + ni;
        for (i, nr) in DENY_SYSCALLS_I386.iter().enumerate() {
            let idx = i386_start + 1 + i;
            assert_eq!(prog[idx].code, BPF_JMP | BPF_JEQ, "i386 deny {i}");
            assert_eq!(prog[idx].k, *nr as u32, "i386 deny {i} syscall id");
            assert_eq!(
                idx + 1 + prog[idx].jt as usize,
                d_i386,
                "i386 deny {i} must terminate at the EPERM terminal"
            );
        }
        assert_eq!(prog[d_i386 - 1].code, BPF_RET | BPF_K);
        assert_eq!(prog[d_i386 - 1].k, RET_ALLOW);
        assert_eq!(prog[d_i386].code, BPF_RET | BPF_K);
        assert_eq!(prog[d_i386].k, RET_ERRNO_EPERM);
        assert_eq!(prog.len(), d_i386 + 1, "no trailing instructions");
    }

    #[test]
    fn denylist_has_no_duplicate_syscalls() {
        for (name, list) in [
            ("x86_64", DENY_SYSCALLS_X86_64),
            ("i386", DENY_SYSCALLS_I386),
        ] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), list.len(), "duplicate deny syscall ({name})");
        }
        // The AF_PACKET sub-branch must not collide with the plain denies.
        assert_eq!(SYS_SOCKET_X86_64, 41);
        assert_eq!(AF_PACKET, 17);
    }

    #[test]
    fn parse_size_handles_suffixes() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("8"), Some(8));
        assert_eq!(parse_size("512m"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("64k"), Some(64 * 1024));
        assert_eq!(parse_size("junk"), None);
    }
}
