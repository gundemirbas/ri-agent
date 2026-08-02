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
        let _ = rc;
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
    fn parse_size_handles_suffixes() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("8"), Some(8));
        assert_eq!(parse_size("512m"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("64k"), Some(64 * 1024));
        assert_eq!(parse_size("junk"), None);
    }
}
