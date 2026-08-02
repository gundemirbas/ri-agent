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
