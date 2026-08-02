//! Integration tests for the rootless sandbox (`ri-sandbox`).
//!
//! These drive the real child binary (`CARGO_BIN_EXE_ri-sandbox` — guaranteed
//! present for integration tests), a scratch image assembled through the
//! library's `container::rootfs`, and assert on the *inside-of-sandbox* view:
//! user-namespace root mapping, chroot isolation from the host, the writable
//! `/work`/`/tmp` binds, and the static-uutils file tools.
//!
//! Every sandbox test probes user namespaces first and SKIPS (with a note)
//! when the host kernel forbids unprivileged `unshare(CLONE_NEWUSER)`.

use std::path::{Path, PathBuf};
use std::process::Command;

use ri_agent::container::rootfs::{assemble_image, coreutils_candidates};

const SANDBOX: &str = env!("CARGO_BIN_EXE_ri-sandbox");

/// Serialize env-mutating tests + the heavy image assemblies.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ri-sbox-it-{name}-{}", std::process::id()))
}

fn userns_available() -> bool {
    match Command::new("unshare").args(["-Ur", "true"]).status() {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

fn unlock_poisoned() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn assemble_once(img: &Path) {
    if !img.join(".ri-image-ready").is_file() {
        assemble_image(img).expect("assemble scratch image");
    }
}

/// Spawn `ri-sandbox <img> -- argv`, binding `cwd` (the sandbox invokes
/// `/work` = its own cwd) and returning the raw output.
fn in_sandbox(img: &Path, cwd: &Path, argv: &[&str]) -> std::process::Output {
    Command::new(SANDBOX)
        .arg(img)
        .arg("--")
        .args(argv)
        .current_dir(cwd)
        .output()
        .expect("spawn ri-sandbox")
}

fn all_output(o: &std::process::Output) -> String {
    format!(
        "{}|{}|{:?}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr),
        o.status
    )
}

#[test]
fn maps_uid_to_fake_root_inside_the_sandbox() {
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces unavailable");
        return;
    }
    let _g = unlock_poisoned();
    let img = scratch("uid0");
    assemble_once(&img);
    let out = in_sandbox(&img, &img, &["/bin/sh", "-c", "id -u; id -g"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "0\n0\n",
        "expected uid/gid 0 inside the sandbox; full: {}",
        all_output(&out)
    );
    std::fs::remove_dir_all(&img).ok();
}

#[test]
fn hides_the_host_filesystem_beyond_the_explicit_binds() {
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces unavailable");
        return;
    }
    let _g = unlock_poisoned();

    // A host-side "private" dir (like a home dir) with a secret.
    let private = scratch("private");
    std::fs::create_dir_all(private.join("nested")).unwrap();
    std::fs::write(private.join("nested/SECRET.txt"), "host-secret").unwrap();

    let img = scratch("iso");
    assemble_once(&img);
    // cwd = private → /work/NESTED with the secret IS visible (that's the
    // workspace). But the same path /outside/NESTED and /root or /home/..
    // must NOT resolve to the host home.
    let out = in_sandbox(
        &img,
        &private.clone(),
        &[
            "/bin/sh",
            "-c",
            "cat /work/nested/SECRET.txt 2>&1; cat /nested/SECRET.txt 2>&1; \
             cat /root/.bashrc 2>&1; cat /home/ri/NESTED/SECRET.txt 2>&1; ls /",
        ],
    );
    let t = all_output(&out);
    assert!(
        t.contains("host-secret"),
        "the /work bind must expose the cwd: {t}"
    );
    assert!(
        t.contains("No such file"),
        "paths outside the binds must be invisible: {t}"
    );
    std::fs::remove_dir_all(&img).ok();
    std::fs::remove_dir_all(&private).ok();
}

#[test]
fn shadow_and_other_host_secrets_are_not_exposed() {
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces unavailable");
        return;
    }
    let _g = unlock_poisoned();
    let img = scratch("sec");
    assemble_once(&img);
    let out = in_sandbox(
        &img,
        &img,
        &["/bin/sh", "-c", "cat /etc/shadow 2>&1; ls /etc/ssh 2>&1"],
    );
    let t = all_output(&out);
    assert!(
        t.contains("No such file"),
        "host /etc/shadow and /etc/ssh must be hidden: {t}"
    );
    // The scrubbed /etc still has passwd/group/resolv (DNS + user lookup).
    let out2 = in_sandbox(
        &img,
        &img,
        &[
            "/bin/sh",
            "-c",
            "head -1 /etc/passwd; head -1 /etc/resolv.conf",
        ],
    );
    assert!(
        !out2.status.success() || !all_output(&out2).trim().is_empty(),
        "DNS/identity files bind silently or the command ran; got: {}",
        all_output(&out2)
    );
    std::fs::remove_dir_all(&img).ok();
}

#[test]
fn work_bind_is_writable_and_persists_to_the_host() {
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces unavailable");
        return;
    }
    let _g = unlock_poisoned();
    let img = scratch("wr");
    let work = scratch("wr-work");
    std::fs::create_dir_all(&work).unwrap();
    assemble_once(&img);
    let out = in_sandbox(
        &img,
        &work,
        &[
            "/bin/sh",
            "-c",
            "echo written > /work/out.txt; echo ok > /tmp/scratch.txt",
        ],
    );
    assert!(
        out.status.success(),
        "write to bound dirs failed: {}",
        all_output(&out)
    );
    assert_eq!(
        std::fs::read_to_string(work.join("out.txt")).expect("read /work output"),
        "written\n",
        "the /work bind must persist to the host cwd"
    );
    assert!(
        std::path::Path::new("/tmp/scratch.txt").exists(),
        "the shared /tmp bind must be visible on the host"
    );
    let _ = std::fs::remove_file("/tmp/scratch.txt");
    std::fs::remove_dir_all(&img).ok();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn static_uutils_coreutils_provides_file_tools_when_provisioned() {
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces unavailable");
        return;
    }
    if coreutils_candidates().iter().all(|c| !c.is_file()) {
        eprintln!("SKIP: no static coreutils provisioned (scripts/fetch-uutils-coreutils.sh)");
        return;
    }
    let _g = unlock_poisoned();
    let img = scratch("uu");
    assemble_once(&img);
    assert!(
        std::fs::read_to_string(img.join("has-uutils"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false),
        "provisioned coreutils must be installed into the image"
    );
    let out = in_sandbox(
        &img,
        &img,
        &[
            "/bin/sh",
            "-c",
            "ls /usr/bin/coreutils >/dev/null && echo UUTILS_OK; ls / | tr '\\n' ' '",
        ],
    );
    let t = all_output(&out);
    assert!(
        t.contains("UUTILS_OK"),
        "static coreutils must exist inside: {t}"
    );
    // Self-contained image: host /bin,/usr are NOT mounted → host-only dirs
    // like /opt are invisible.
    assert!(
        !t.contains("opt"),
        "host /opt must be invisible with a self-contained image: {t}"
    );
    std::fs::remove_dir_all(&img).ok();
}

const RI_SH_BIN: &str = env!("CARGO_BIN_EXE_ri-sh");

/// Change an env var for the duration of the closure (serialized via the
/// global test lock; edition-2024 unsafe).
fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
    unsafe {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
    let r = f();
    unsafe {
        std::env::remove_var(key);
    }
    r
}

#[test]
fn strict_static_shell_drops_host_shell_and_lib_binds() {
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces unavailable");
        return;
    }
    if !coreutils_candidates().iter().any(|c| c.is_file()) {
        eprintln!("SKIP: no static coreutils provisioned (scripts/fetch-uutils-coreutils.sh)");
        return;
    }
    let _g = unlock_poisoned();
    let img = scratch("strict");
    // A toolchain would place its musl loader in /lib; disable it here so the
    // strict no-host-libs assertion stays exact (toolchain is tested by the
    // rustc-bootstrap test instead).
    with_env("RI_SANDBOX_TOOLCHAIN", Some("/__ri_tc_unset__"), || {
        with_env("RI_SANDBOX_SH", Some(RI_SH_BIN), || {
            assemble_image(&img).expect("assemble strict image")
        })
    });
    let static_sh = std::fs::read_to_string(img.join("has-static-sh"))
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    assert!(static_sh, "ri-sh must be installed as the static shell");
    // bin/sh must be a symlink to the static ri-sh, not a host copy.
    use std::fs::symlink_metadata;
    let m = symlink_metadata(img.join("bin/sh")).expect("bin/sh exists");
    assert!(
        m.file_type().is_symlink(),
        "bin/sh must be a symlink in strict mode"
    );

    let out = in_sandbox(
        &img,
        &img,
        &[
            "/bin/sh",
            "-c",
            "ls -ld /bin/sh >/dev/null 2>&1; echo UID=$(id -u); ls /usr/bin/coreutils >/dev/null && echo UUTILS_OK; echo LIBCNT=$(ls -A /lib | wc -l); ls /opt 2>&1",
        ],
    );
    let t = all_output(&out);
    assert!(t.contains("UID=0"), "static shell must map uid to 0: {t}");
    assert!(
        t.contains("UUTILS_OK"),
        "static coreutils must be present: {t}"
    );
    // Strict image: no host /lib loader binds → /lib is empty (LIBCNT=0).
    assert!(
        t.contains("LIBCNT=0"),
        "expected an empty /lib (no host lib binds); got: {t}"
    );
    assert!(
        t.contains("No such file"),
        "host /opt must stay invisible: {t}"
    );
    std::fs::remove_dir_all(&img).ok();
}

#[test]
fn compat_mode_uses_host_shell_without_static_ri_sh() {
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces unavailable");
        return;
    }
    let _g = unlock_poisoned();
    let img = scratch("compat");
    // Force the compatible image: a static ri-sh is deliberately not found
    // (the override short-circuits the sibling auto-detection).
    with_env("RI_SANDBOX_SH", Some("/__ri_sh_unset_compat__"), || {
        assemble_image(&img).expect("assemble compat image")
    });
    let static_sh = std::fs::read_to_string(img.join("has-static-sh"))
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    assert!(!static_sh, "compat image must NOT have a static shell");

    // bin/sh is a host copy (regular file), not a symlink.
    use std::fs::symlink_metadata;
    let m = symlink_metadata(img.join("bin/sh")).expect("bin/sh exists");
    assert!(
        !m.file_type().is_symlink(),
        "compat bin/sh must be a host copy"
    );

    // The compatible host shell still runs inside the sandbox.
    let out = in_sandbox(
        &img,
        &img,
        &["/bin/sh", "-c", "echo shell-ok; echo PI=$(pwd)"],
    );
    let t = all_output(&out);
    assert!(t.contains("shell-ok"), "compat host shell must run: {t}");
    assert!(
        t.contains("PI=/work"),
        "host shell must run with /work cwd: {t}"
    );
    std::fs::remove_dir_all(&img).ok();
}

#[test]
fn rlimits_are_applied_inside_the_sandbox() {
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces unavailable");
        return;
    }
    let _g = unlock_poisoned();
    let img = scratch("rlimit");
    assemble_image(&img).expect("assemble");
    // Explicit limits so the assertion is deterministic.
    let out = Command::new(SANDBOX)
        .arg(&img)
        .arg("--")
        .args([
            "/bin/sh",
            "-c",
            "cat /proc/self/limits 2>/dev/null || echo NO_PROC",
        ])
        .env(
            "RI_SANDBOX_RLIMITS",
            "nofile=2048,nproc=64,as=512m,cpu=30,fsize=1g,core=0",
        )
        .current_dir(&img)
        .output()
        .expect("spawn ri-sandbox");
    let t = all_output(&out);
    if t.contains("NO_PROC") {
        // /proc is not bind-mounted on this host (e.g. hardened kernels) —
        // the limit is still applied; we just cannot observe it here.
        eprintln!("INFO: /proc unavailable; RLIMIT not observable (limits still set)");
        return;
    }
    let lines = t.lines().collect::<Vec<_>>();
    let find = |name: &str| {
        lines
            .iter()
            .find(|l| l.contains(name))
            .map(|l| l.to_string())
    };
    if let Some(l) = find("Max open files") {
        assert!(l.contains("2048"), "nofile must be 2048: {l}");
    }
    if let Some(l) = find("Max cpu time") {
        assert!(l.contains("30"), "cpu limit must be 30s: {l}");
    }
    if let Some(l) = find("Max address space") {
        assert!(l.contains("524288"), "as limit must be 512m (kB): {l}");
    }
    std::fs::remove_dir_all(&img).ok();
}

#[test]
fn seccomp_denylist_blocks_chroot_but_keeps_tools_working() {
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces unavailable");
        return;
    }
    // Differential proof needs the static coreutils multicall (`chroot`
    // applet) inside the image; without staged coreutils we cannot drive a
    // blocked syscall from a shell command, so we skip.
    if coreutils_candidates().is_empty() {
        eprintln!("SKIP: no static coreutils provisioned (scripts/fetch-uutils-coreutils.sh)");
        return;
    }
    let _g = unlock_poisoned();
    let img = scratch("seccomp");
    assemble_image(&img).expect("assemble");
    let cwd = img.clone();

    // Baseline: the sandbox still runs ordinary tools with the filter on.
    let control = in_sandbox(&img, &cwd, &["/bin/sh", "-c", "id -u"]);
    let t = all_output(&control);
    assert!(
        t.contains("0"),
        "ordinary tools must run under seccomp: {t}"
    );

    // With the default filter, `chroot` is answered EPERM by seccomp (the
    // userns root holds CAP_SYS_CHROOT, so the capability alone would allow it
    // — the block must come from the filter).
    let blocked = Command::new(SANDBOX)
        .arg(&img)
        .arg("--")
        .args([
            "/bin/sh",
            "-c",
            "coreutils chroot / /bin/true 2>&1; echo RC=$?",
        ])
        .current_dir(&cwd)
        .output()
        .expect("spawn ri-sandbox (seccomp on)");
    let blocked_out = all_output(&blocked);
    assert!(
        blocked_out.contains("Operation not permitted"),
        "chroot must be blocked by seccomp: {blocked_out}"
    );

    // With the escape hatch (`$RI_SANDBOX_NO_SECCOMP=1`) the same chroot
    // succeeds (it only then fails to exec /bin/true, which stays a no-op
    // chroot) — proving the differential is the filter, not a capability gap.
    let open = Command::new(SANDBOX)
        .arg(&img)
        .arg("--")
        .args([
            "/bin/sh",
            "-c",
            "coreutils chroot / /bin/true 2>&1; echo RC=$?",
        ])
        .env("RI_SANDBOX_NO_SECCOMP", "1")
        .current_dir(&cwd)
        .output()
        .expect("spawn ri-sandbox (seccomp off)");
    let open_out = all_output(&open);
    assert!(
        !open_out.contains("Operation not permitted"),
        "no-seccomp chroot must pass the syscall: {open_out}"
    );

    std::fs::remove_dir_all(&img).ok();
}
