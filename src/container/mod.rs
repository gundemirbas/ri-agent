//! Rootless, image-less container runtime for tool execution.
//!
//! Implements the design in `docs/CONTAINER-RUNTIME-SPEC.md` (scaled for this
//! repo — see the spec's "Implementation status" appendix):
//!
//! - **No OCI / podman / docker / root.** Each tool runs inside a fresh
//!   **user namespace** (`unshare(CLONE_NEWUSER | CLONE_NEWNS)`) where the
//!   invoking uid is mapped to uid 0, then is `chroot`ed into a flat,
//!   non-image **rootfs directory** with only a scrubbed view of the host.
//! - **Images**: a lazily assembled flat rootfs (`~/.cache/ri/sandbox/rootfs`
//!   unless `$RI_SANDBOX_IMAGE` or the XDG cache says otherwise). File tools
//!   come from a **static musl uutils coreutils** binary when one is
//!   provisioned (see `scripts/fetch-uutils-coreutils.sh`); otherwise the
//!   host's `/bin`, `/usr` are bind-mounted read-only as a fallback so the
//!   sandbox always works.
//! - **Isolation**: `/home`, `/root`, `/etc` (host secrets), and other
//!   projects are invisible. Only explicit binds are visible/writable:
//!   `/work` (the tool cwd), `/tools` (custom tools), `/tmp` (scratch), plus
//!   the non-secret `/etc/*` files needed for DNS/users and the dynamic
//!   loader dirs for the shell.
//! - **Network**: the network namespace is *not* isolated — host network is
//!   shared, so tools keep internet access (crates.io, APIs, …).
//!
//! The sandbox child itself is the `ri-sandbox` bin target (single-threaded
//! pre-tokio main so `unshare(CLONE_NEWUSER)` is legal). Tool calls are routed
//! through it by [`crate::agent::tools::subprocess`] whenever the sandbox flag
//! is enabled on the tool executor (`--sandbox` / `sandbox = true` in
//! config.toml). Linux-only; on other platforms enabling it fails loudly.

pub mod rootfs;
pub mod sys;
#[cfg(test)]
mod tests;

/// Serializes shell/environment-mutating sandbox tests (`RI_SANDBOX_*` are
/// process-global). Async-aware so the guard can be held across awaits; unit
/// tests in the bin crate that touch these env vars must take it.
// Only the bin crate's unit tests (subprocess/tools::rustc sandbox tests)
// take this lock; the library target compiles it too but has no caller, so the
// dead-code lint cannot see its real use.
#[allow(dead_code)]
pub(crate) static SANDBOX_ENV_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Resolve the on-disk sandbox image root.
///
/// Order: `$RI_SANDBOX_IMAGE` (tests/overrides) → XDG cache
/// `ri/sandbox/rootfs` → `~/.cache/ri/sandbox/rootfs` → `/tmp/ri-sandbox`.
pub fn image_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("RI_SANDBOX_IMAGE") {
        return PathBuf::from(p);
    }
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("ri").join("sandbox").join("rootfs");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("ri")
            .join("sandbox")
            .join("rootfs");
    }
    PathBuf::from("/tmp/ri-sandbox")
}

/// Assemble the sandbox image if needed and return its root path.
///
/// Failures here mean the sandbox cannot be used, and the caller should
/// refuse to start with the sandbox enabled.
pub fn ensure_initialized() -> io::Result<PathBuf> {
    let image = image_dir();
    rootfs::assemble_image(&image)?;
    Ok(image)
}

/// Absolute path to the `ri-sandbox` child binary.
///
/// Order: `$RI_SANDBOX_BIN` (tests), the running binary's sibling (the usual
/// `target/<profile>/ri-sandbox` beside `target/<profile>/ri`), or the parent
/// profile dir of the test harness (`target/<profile>`).
pub fn sandbox_bin_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RI_SANDBOX_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("ri-sandbox");
        if sibling.is_file() {
            return sibling;
        }
        if let Some(parent) = dir.parent()
            && let prof = parent.join("ri-sandbox")
            && prof.is_file()
        {
            return prof;
        }
    }
    PathBuf::from("ri-sandbox")
}

/// Rewrite a tool invocation so it runs inside the sandbox child.
///
/// - `program`/`args` become `ri-sandbox <image> -- <program> <args...>`.
/// - A program that lives in one of `custom_tool_dirs` is rewritten to its
///   guest path (`/tools/<name>` or `/tools-local/<name>`), since the host
///   absolute path does not exist inside the chroot.
///
/// Returns the child binary path and its argv.
pub fn sandbox_argv(
    program: &str,
    args: &[String],
    custom_tool_dirs: &[(&Path, &str)],
) -> (String, Vec<String>) {
    let mut mapped = program.to_string();
    for (host_dir, guest_dir) in custom_tool_dirs {
        if let Ok(hd) = host_dir.canonicalize()
            && let Ok(pp) = Path::new(program).canonicalize()
            && let Ok(rel) = pp.strip_prefix(&hd)
        {
            mapped = format!("{guest_dir}/{}", rel.display());
            break;
        }
    }
    // NB: `Command::new` supplies argv[0] (the child path), so argv[0] here
    // is the image path; the child parses `<image> -- <program> <args…>`.
    let mut argv = vec![
        image_dir().to_string_lossy().into_owned(),
        "--".to_string(),
        mapped,
    ];
    argv.extend(args.iter().cloned());
    (sandbox_bin_path().to_string_lossy().into_owned(), argv)
}
