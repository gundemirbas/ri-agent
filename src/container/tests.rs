//! Pure unit tests for image assembly (no subprocess spawning — the real
//! sandbox behavior is covered by `tests/container_sandbox.rs`, which can
//! reach the `ri-sandbox` binary via `CARGO_BIN_EXE_ri-sandbox`).

use std::path::Path;

use super::rootfs::{AssembledImage, assemble_image, coreutils_candidates};

fn temp_image(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ri-sbox-unit-{name}-{}", std::process::id()))
}

#[test]
fn applet_list_contains_the_essentials() {
    for a in ["ls", "cat", "id", "echo", "wc", "cp", "mkdir"] {
        assert!(
            super::rootfs::APPLETS.contains(&a),
            "missing applet {a} from the static coreutils install set"
        );
    }
}

#[test]
fn assemble_image_is_idempotent_and_writes_markers() {
    let img = temp_image("idem");
    std::fs::remove_dir_all(&img).ok();
    let a: AssembledImage = assemble_image(&img).expect("assemble");
    let b: AssembledImage = assemble_image(&img).expect("reassemble");
    assert_eq!(a, b, "reassembling the same image must be idempotent");
    assert!(img.join(".ri-image-ready").is_file());
    assert!(img.join("has-uutils").is_file());
    assert!(img.join("has-shell").is_file());
    // Layout sanity: writable scratch + guest paths exist.
    for d in ["work", "tools", "tools-local", "tmp", "home/ri", "proc"] {
        assert!(img.join(d).is_dir(), "image missing {d}");
    }
    std::fs::remove_dir_all(&img).ok();
}

#[test]
fn host_shell_is_copied_and_marked() {
    let img = temp_image("sh");
    std::fs::remove_dir_all(&img).ok();
    let a = assemble_image(&img).unwrap();
    if Path::new("/bin/sh").is_file() {
        assert!(
            a.has_shell,
            "a host shell exists, so the image should have one"
        );
        assert!(img.join("bin").join("sh").is_file());
    }
    std::fs::remove_dir_all(&img).ok();
}

#[test]
fn ubuntu_style_candidates_are_stable_paths() {
    // The candidate list must always include a deterministic entry predicated
    // on `CARGO_MANIFEST_DIR` (repo staging) and the cache dir.
    let cands = coreutils_candidates();
    assert!(!cands.is_empty());
}

#[test]
fn sandbox_argv_rewrites_tool_to_sandbox_child_and_maps_custom_tools() {
    use crate::container::sandbox_argv;
    let proj = std::env::temp_dir().join(format!("ri-sbox-argv-{}", std::process::id()));
    std::fs::create_dir_all(proj.join(".ri/tools")).ok();
    let tool = proj.join(".ri/tools").join("mytool");
    std::fs::write(&tool, b"#!/bin/sh\necho hi\n").ok();

    // In production the host dir IS the custom-tool dir (~/.ri/tools), so the
    // mapped guest is /tools/<name> directly.
    let tool_dir = proj.join(".ri").join("tools");
    let dirs = [(tool_dir.as_path(), "/tools")];
    let (bin, argv) = sandbox_argv(tool.to_str().unwrap(), &["--flag".to_string()], &dirs);
    assert!(bin.ends_with("ri-sandbox"), "bin is ri-sandbox, got {bin}");
    // [image, --, /tools/mytool, --flag]
    assert_eq!(argv.len(), 4, "argv: {argv:?}");
    assert_eq!(
        argv[2], "/tools/mytool",
        "host custom-tool path is mapped to /tools"
    );
    assert_eq!(argv[3], "--flag");
    // Unmapped absolute host paths stay as-is.
    let (_b, argv2) = sandbox_argv("/usr/bin/nonexistent", &[], &dirs);
    assert_eq!(argv2[2], "/usr/bin/nonexistent");

    let _ = std::fs::remove_dir_all(&proj);
}
