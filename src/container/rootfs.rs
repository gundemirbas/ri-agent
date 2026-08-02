//! Sandbox image assembly.
//!
//! The image is a flat, non-OCI rootfs directory:
//!
//! ```text
//! <image>/
//! ├── bin/sh        ← copied from host `/bin/sh` (dynamic; its loader/lib
//! │                    dirs are bind-mounted read-only at runtime)
//! ├── usr/bin/coreutils ← static musl uutils coreutils (when provisioned)
//! │             + `<applet>` symlinks → coreutils (ls, cat, cp, …)
//! ├── usr/bin/|lib/|lib64/ … ← runtime bind targets
//! ├── etc/passwd|group|resolv.conf|hosts … (scrubbed, non-secret)
//! ├── proc/ tmp/ work/ tools/ home/ri …
//! └── .ri-image-ready   · has-uutils        ← markers
//! ```
//!
//! `assemble_image` is idempotent and cheap (a marker check). File utilities
//! are resolved in this order (first match wins, the rest are skipped):
//! host-*fallback* only when no static coreutils is available.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Applet symlinks installed for the static coreutils multicall binary.
pub const APPLETS: &[&str] = &[
    "basename",
    "cat",
    "chgrp",
    "chmod",
    "chown",
    "cp",
    "cut",
    "date",
    "dd",
    "df",
    "dircolors",
    "dirname",
    "du",
    "echo",
    "env",
    "expand",
    "expr",
    "factor",
    "false",
    "fmt",
    "fold",
    "groups",
    "head",
    "id",
    "install",
    "join",
    "link",
    "ln",
    "logname",
    "ls",
    "md5sum",
    "mkdir",
    "mkfifo",
    "mknod",
    "mktemp",
    "mv",
    "nice",
    "nl",
    "nohup",
    "nproc",
    "numfmt",
    "od",
    "paste",
    "pathchk",
    "pinky",
    "pr",
    "printenv",
    "printf",
    "ptx",
    "pwd",
    "readlink",
    "realpath",
    "rm",
    "rmdir",
    "seq",
    "sha1sum",
    "sha224sum",
    "sha256sum",
    "sha384sum",
    "sha512sum",
    "shred",
    "shuf",
    "sleep",
    "sort",
    "split",
    "stat",
    "stty",
    "sum",
    "sync",
    "tac",
    "tail",
    "tee",
    "test",
    "touch",
    "tr",
    "true",
    "truncate",
    "tsort",
    "tty",
    "uname",
    "unexpand",
    "uniq",
    "unlink",
    "users",
    "vdir",
    "wc",
    "who",
    "whoami",
    "yes",
];

/// Directories that must exist inside the image before any mount. Paths that
/// mirror host locations (loader dirs) are created once and filled by runtime
/// bind mounts; `work/tools/tmp/home` are the writable scratch mounts.
const IMAGE_DIRS: &[&str] = &[
    "bin",
    "sbin",
    "usr/bin",
    "usr/sbin",
    "usr/lib",
    "usr/lib64",
    "usr/lib32",
    "usr/share",
    "lib",
    "lib64",
    "lib32",
    "etc/ssl",
    "proc",
    "dev",
    "tmp",
    "work",
    "tools",
    "tools-local",
    "toolchain",
    "x86_64-linux-musl-cross",
    "home/ri",
    "nix/store",
    "run/current-system/sw",
];

/// Non-secret `/etc` files the sandbox binds from the host (files get a
/// placeholder; the runtime bind replaces their contents).
const ETC_BINDS: &[&str] = &["passwd", "group", "resolv.conf", "nsswitch.conf", "hosts"];

/// Marker file written when assembly finishes; its presence means the image
/// layout is valid and owned by this build.
const READY_MARKER: &str = ".ri-image-ready";
/// Marker written when a static coreutils was installed (1) or not (0).
const UUTILS_MARKER: &str = "has-uutils";
/// Marker written when a shell is present in the image (0/1).
const SHELL_MARKER: &str = "has-shell";
/// Marker written when the shell is a static musl binary (ri-sh, 0/1).
const STATIC_SH_MARKER: &str = "has-static-sh";
/// Marker written when a self-owned Rust toolchain is bound at /toolchain.
const TOOLCHAIN_MARKER: &str = "has-toolchain";
/// Host path of the bound toolchain (written next to [`TOOLCHAIN_MARKER`]).
const TOOLCHAIN_DIR_FILE: &str = "toolchain-dir";

/// The (sole) toolchain target ri's sandbox builds custom tools for.
pub const TOOLCHAIN_TRIPLE: &str = "x86_64-unknown-linux-musl";

/// Result of assembling an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssembledImage {
    /// A static musl coreutils is installed; the runtime may omit the host
    /// `/bin`/`/usr` fallback binds and rely on the self-contained file tools.
    pub has_uutils: bool,
    /// A shell exists in the image. When absent, the runtime binds host
    /// `/bin` so `/bin/sh` still works.
    pub has_shell: bool,
    /// The shell is the static musl `ri-sh` (built on the embeddable `epsh`
    /// POSIX-shell library). When true AND `has_uutils`, the image is fully
    /// self-contained: no host `/bin/sh` copy and no `/lib` binds at all.
    pub has_static_sh: bool,
    /// A self-owned musl-host Rust toolchain is bound read-only at
    /// `/toolchain` (see the agent's `rustc` tool and spec §16). Requires the
    /// loader/gcc-runtime files so rustc itself runs inside the chroot.
    pub has_toolchain: bool,
}

/// Candidate locations for a static coreutils binary, in priority order.
///
/// 1. `$RI_SANDBOX_UUTILS` (explicit override).
/// 2. `$CARGO_MANIFEST_DIR/rootfs/usr/bin/coreutils` (repo provisioning
///    staging, populated by `scripts/fetch-uutils-coreutils.sh`).
/// 3. `~/.cache/ri/sandbox/coreutils` / XDG cache (pre-fetched copy).
pub fn coreutils_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("RI_SANDBOX_UUTILS") {
        out.push(PathBuf::from(p));
    }
    // Compile-time manifest dir: `CARGO_MANIFEST_DIR` is not present as a
    // runtime environment variable.
    out.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("rootfs")
            .join("usr")
            .join("bin")
            .join("coreutils"),
    );
    let cache = if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(x).join("ri").join("sandbox")
    } else {
        std::env::var("HOME")
            .ok()
            .map(PathBuf::from)
            .map(|h| h.join(".cache").join("ri").join("sandbox"))
            .unwrap_or_else(|| PathBuf::from("/tmp/ri-sandbox-cache"))
    };
    out.push(cache.join("coreutils"));
    out
}

/// Candidate locations for a static `ri-sh` (POSIX shell) binary.
///
/// 1. `$RI_SANDBOX_SH` (explicit override).
/// 2. Siblings of the running binary (`target/<profile>/ri-sh` next to
///    `ri`, `ri-sandbox`), or the parent profile dir of the test harness.
pub fn sh_candidates() -> Vec<PathBuf> {
    // An explicit override short-circuits the auto-detection: useful both to
    // force a specific ri-sh and (by pointing at a non-existent path) to force
    // the compatible host-shell image in tests.
    if let Some(p) = std::env::var_os("RI_SANDBOX_SH") {
        return vec![PathBuf::from(p)];
    }
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.join("ri-sh"));
        if let Some(parent) = dir.parent() {
            out.push(parent.join("ri-sh"));
        }
    }
    out
}

/// Install a static `ri-sh` shell into `<image>/bin/ri-sh` and expose it as
/// `bin/sh` (symlink). Returns whether one was found and installed. When
/// installed, the runtime needs no dynamic host shell and no `/lib` binds.
fn install_static_sh(image: &Path) -> io::Result<bool> {
    let Some(src) = sh_candidates().into_iter().find(|c| c.is_file()) else {
        return Ok(false);
    };
    let dst = image.join("bin").join("ri-sh");
    let _ = fs::remove_file(&dst);
    fs::copy(&src, &dst)?;
    fs::set_permissions(&dst, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    fs::write(
        image.join("bin").join(".ri-sh-source"),
        src.to_string_lossy().as_bytes(),
    )?;
    // `bin/sh` → `ri-sh` (the bash tool invokes `/bin/sh -c …`).
    let sh_link = image.join("bin").join("sh");
    let _ = fs::remove_file(&sh_link);
    std::os::unix::fs::symlink("ri-sh", &sh_link)?;
    Ok(true)
}

/// Candidate locations for the self-owned Rust toolchain.
///
/// 1. `$RI_SANDBOX_TOOLCHAIN` (explicit; short-circuits detection).
/// 2. `<repo>/rootfs/toolchain` — populated by
///    `scripts/fetch-rust-toolchain.sh` (NOT the system rustup).
/// 3. `~/.cache/ri/sandbox/toolchain` (a relocated copy).
pub fn toolchain_candidates() -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os("RI_SANDBOX_TOOLCHAIN") {
        return vec![PathBuf::from(p)];
    }
    let mut out: Vec<PathBuf> = Vec::new();
    out.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("rootfs")
            .join("toolchain"),
    );
    let cache = if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(x).join("ri").join("sandbox")
    } else {
        std::env::var("HOME")
            .ok()
            .map(PathBuf::from)
            .map(|h| h.join(".cache").join("ri").join("sandbox"))
            .unwrap_or_else(|| PathBuf::from("/tmp/ri-sandbox-cache"))
    };
    out.push(cache.join("toolchain"));
    out
}

/// Whether `dir` looks like a usable toolchain for [`TOOLCHAIN_TRIPLE`]:
/// a `bin/rustc`, its sysroot `lib/rustlib/<triple>`, and the two runtime
/// libs the musl-host rustc needs inside the chroot (musl loader + gcc
/// runtime for `librustc_driver`).
pub fn toolchain_valid(dir: &Path) -> bool {
    dir.join("bin").join("rustc").is_file()
        && dir
            .join("lib")
            .join("rustlib")
            .join(TOOLCHAIN_TRIPLE)
            .is_dir()
        && dir.join("lib").join("ld-musl-x86_64.so.1").is_file()
        && dir.join("lib").join("libgcc_s.so.1").is_file()
}

/// Record a discovered toolchain for the runtime and prepare bind targets.
fn install_toolchain(image: &Path) -> io::Result<bool> {
    let Some(dir) = toolchain_candidates()
        .into_iter()
        .find(|d| toolchain_valid(d))
    else {
        return Ok(false);
    };
    fs::write(image.join(TOOLCHAIN_MARKER), "1")?;
    fs::write(
        image.join(TOOLCHAIN_DIR_FILE),
        dir.to_string_lossy().as_bytes(),
    )?;
    // Bind targets inside the image for the two runtime libs (the toolchain
    // itself is bound read-only at /toolchain by the ri-sandbox child).
    let lib = image.join("lib").join("ld-musl-x86_64.so.1");
    if !lib.exists() {
        fs::write(&lib, "")?;
    }
    let gcc = image.join("usr/lib").join("libgcc_s.so.1");
    if !gcc.exists() {
        fs::write(&gcc, "")?;
    }
    Ok(true)
}

/// Idempotently assemble the sandbox image at `image`.
pub fn assemble_image(image: &Path) -> io::Result<AssembledImage> {
    // Always (re)build the layout: cheap, and guarantees the image matches the
    // current ri build + provisioned assets (no stale-cache surprises; the
    // toolchain is bind-mounted below, never copied into the image).

    // Layout.
    for d in IMAGE_DIRS {
        fs::create_dir_all(image.join(d))?;
    }

    // Device-node placeholders under /dev (the runtime binds the host's real
    // nodes over them best-effort; a shell needs at least /dev/null).
    for dev in ["null", "zero", "random", "urandom", "tty", "full"] {
        let p = image.join("dev").join(dev);
        if !p.exists() {
            fs::write(&p, "")?;
        }
    }

    // Scrubbed /etc placeholders (contents replaced by runtime binds; these
    // also serve as bind targets).
    for f in ETC_BINDS {
        let p = image.join("etc").join(f);
        if !p.is_file() {
            fs::write(&p, "")?;
        }
    }
    // Minimal /etc/hosts fallback if the host bind is unavailable.
    let hosts = image.join("etc").join("hosts");
    if fs::read_to_string(&hosts)
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        fs::write(
            &hosts,
            "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n",
        )?;
    }

    // Shell: prefer the static `ri-sh` binary (built on the embeddable `epsh`
    // POSIX-shell library); only when it is unavailable copy the host's
    // (dynamic) `/bin/sh`, whose interpreter/library dirs the runtime then
    // binds read-only.
    let has_static_sh = install_static_sh(image)?;
    let has_host_sh = if !has_static_sh {
        copy_host_shell(image)?
    } else {
        false
    };
    let has_shell = has_static_sh || has_host_sh;

    // Static coreutils (file tools) — self-contained, no host binds needed.
    let has_uutils = install_coreutils(image)?;

    // Self-owned Rust toolchain (compiled custom tools; spec §6/§16).
    let has_toolchain = install_toolchain(image)?;

    // Markers.
    fs::write(
        image.join(UUTILS_MARKER),
        if has_uutils { "1" } else { "0" },
    )?;
    fs::write(image.join(SHELL_MARKER), if has_shell { "1" } else { "0" })?;
    fs::write(
        image.join(STATIC_SH_MARKER),
        if has_static_sh { "1" } else { "0" },
    )?;
    fs::write(image.join(READY_MARKER), env!("CARGO_PKG_VERSION"))?;

    Ok(AssembledImage {
        has_uutils,
        has_shell,
        has_static_sh,
        has_toolchain,
    })
}

/// Copy the host `/bin/sh` into `<image>/bin/sh`. Returns whether a shell is
/// present (`/bin/sh` existed on the host).
fn copy_host_shell(image: &Path) -> io::Result<bool> {
    let src = Path::new("/bin/sh");
    let dst = image.join("bin").join("sh");
    if !src.is_file() {
        return Ok(false);
    }
    // fs::copy propagates the source mode (usually 0555, owner-UNwritable), so
    // a re-assembly would hit EACCES trying to O_TRUNC it. Remove + re-copy.
    let _ = fs::remove_file(&dst);
    fs::copy(src, &dst)?;
    // Give the owner write so future copies / rebuilds are not blocked either.
    fs::set_permissions(&dst, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    Ok(true)
}

/// Install a static coreutils binary + applet symlinks into the image.
/// Returns whether one was found and installed.
fn install_coreutils(image: &Path) -> io::Result<bool> {
    let Some(src) = coreutils_candidates().into_iter().find(|c| c.is_file()) else {
        return Ok(false);
    };

    let dst = image.join("usr").join("bin").join("coreutils");
    let _ = fs::remove_file(&dst);
    // The multicall dispatches on argv[0], so applet symlinks reuse the single
    // binary (busybox-style); `coreutils <applet>` also works when invoked
    // directly.
    fs::copy(&src, &dst)?;
    fs::set_permissions(&dst, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    fs::write(
        image.join("usr").join("bin").join(".coreutils-source"),
        src.to_string_lossy().as_bytes(),
    )?;
    for applet in APPLETS {
        let link = image.join("usr").join("bin").join(applet);
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink("coreutils", &link)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applet_list_contains_shell_fixtures() {
        for a in ["ls", "cat", "id", "echo", "wc"] {
            assert!(APPLETS.contains(&a), "missing applet {a}");
        }
    }

    #[test]
    fn assemble_image_is_idempotent_and_marks() {
        let dir =
            std::env::temp_dir().join(format!("ri-sandbox-rootfs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let a = assemble_image(&dir).expect("assemble");
        let b = assemble_image(&dir).expect("reassemble");
        assert_eq!(a, b);
        assert!(dir.join(READY_MARKER).is_file());
        assert!(dir.join("usr/bin").is_dir());
        assert!(dir.join("work").is_dir());
        let _ = fs::remove_dir_all(&dir);
    }
}
