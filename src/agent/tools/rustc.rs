//! The `rustc` tool — compile a custom tool inside the sandbox (bootstrapping).
//!
//! Implements `docs/CONTAINER-RUNTIME-SPEC.md` §6: the agent supplies a Rust
//! **single-file source**; the tool compiles it **inside the sandbox** to a
//! static musl binary and installs it into the host custom-tools directory
//! (`~/.ri/tools`), where `load_custom_tools()` picks it up so the agent can
//! invoke it on a following turn.
//!
//! The compiler is the sandbox's self-owned **musl-host** Rust toolchain
//! (`scripts/fetch-rust-toolchain.sh` → bound read-only at `/toolchain`), NOT
//! the system rustup. Pure-Rust (std-only) sources need no musl dev libraries:
//! Rust's `-C link-self-contained=yes` ships `libc.a` + CRT + `libunwind.a`
//! for the target. Sources that compile/link native C (the `cc` crate, C
//! system libraries) need a musl C cross toolchain, which is a future
//! provisioning step (see the spec §16 note).

use std::pin::Pin;

use serde_json::Value;

use super::custom::custom_tool_dirs;
use super::subprocess::SubprocessCommand;
use crate::agent::types::{Tool, ToolCallContext, ToolResult};
use crate::container::rootfs::{TOOLCHAIN_TRIPLE, toolchain_candidates, toolchain_valid};

/// Path of the bundled LINKER inside the sandbox (rust-lld, no external ld).
pub const RUST_LD: &str = "/toolchain/lib/rustlib/x86_64-unknown-linux-musl/bin/rust-lld";

#[derive(serde::Deserialize)]
struct RustcArgs {
    /// The complete Rust source of the tool (uses std + no crates).
    source: String,
    /// Output tool name (filename inside the tools dir). Optional; derived
    /// from the first `fn` if absent.
    #[serde(default)]
    name: Option<String>,
}

pub struct RustcTool;

/// Sanitize a user-supplied tool name into a safe filename token.
fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches(['.', '-', '_']);
    if out.is_empty() {
        "ri_tool".to_string()
    } else {
        out.to_string()
    }
}

/// Bootstrapped tool sources must implement the custom-tool protocol the
/// loaded tools use: `--describe` → JSON descriptor, JSON on stdin → result.
const PROTOCOL_HINT: &str = "the compiled tool must implement the custom-tool \
                             protocol: `--describe` prints a JSON descriptor, \
                             JSON args on stdin produce a result on stdout. \
                             It becomes active on the next tool (re)load.";

impl Tool for RustcTool {
    fn name(&self) -> &str {
        "rustc"
    }

    fn description(&self) -> &str {
        "Compile a single-file Rust program into a static musl custom tool \
         inside the sandbox and install it in the custom-tools directory \
         (~/.ri/tools). The tool then follows the custom-tool protocol: \
         `--describe` prints a JSON descriptor; JSON on stdin returns a result \
         on stdout. Requires the sandbox (--sandbox) and the provisioned Rust \
         toolchain."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Complete Rust 2021 source of the tool (std only, no external crates). Implement fn main() that parses an optional --describe flag and JSON args on stdin."
                },
                "name": {
                    "type": "string",
                    "description": "Optional output tool name (filename sans extension)."
                }
            },
            "required": ["source"]
        })
    }

    fn streaming_field(&self) -> Option<&'static str> {
        Some("source")
    }

    fn run(
        &self,
        args: Value,
        ctx: ToolCallContext,
    ) -> Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            if !ctx.sandbox {
                return ToolResult::err(
                    "the `rustc` bootstrap tool compiles inside the sandbox; \
                     start ri with --sandbox (or sandbox = true in config.toml)",
                );
            }
            let parsed: RustcArgs = match serde_json::from_value(args) {
                Ok(p) => p,
                Err(e) => return ToolResult::err(format!("bad arguments: {e}")),
            };

            // The compiler must be provisioned (self-owned toolchain; env /
            // repo-staging / cache — see rootfs::toolchain_candidates).
            if !toolchain_candidates().iter().any(|d| toolchain_valid(d)) {
                return ToolResult::err(
                    "no Rust toolchain provisioned: run \
                     `scripts/fetch-rust-toolchain.sh` (see \
                     docs/CONTAINER-RUNTIME-SPEC.md §16)",
                );
            }

            let name = sanitize_name(parsed.name.as_deref().unwrap_or("ri_tool"));

            // Host-side tools dir that /tools maps to (custom_tool_dirs()[0]
            // is the shared tools dir; ensure it exists so the bind has a
            // target).
            let tools_host = match custom_tool_dirs().into_iter().next() {
                Some(d) => d,
                None => {
                    return ToolResult::err("no custom-tools directory configured (~/.ri/tools)");
                }
            };
            if let Err(e) = std::fs::create_dir_all(&tools_host) {
                return ToolResult::err(format!("cannot create {}: {e}", tools_host.display()));
            }
            let out_host = tools_host.join(&name);

            // Write the source into the shared /tmp (inside the sandbox it is
            // visible at the same absolute path via the /tmp bind).
            let src_host =
                std::env::temp_dir().join(format!("ri-rustc-{}-{name}.rs", std::process::id()));
            if let Err(e) = std::fs::write(&src_host, parsed.source.as_bytes()) {
                return ToolResult::err(format!("cannot write source: {e}"));
            }

            let rustc = "/toolchain/bin/rustc";
            let mut cmd = SubprocessCommand::new(rustc);
            cmd = cmd
                .arg("--target")
                .arg(TOOLCHAIN_TRIPLE)
                .sandboxed(ctx.sandbox);
            for flag in [
                "link-self-contained=yes",
                "linker-flavor=ld.lld",
                "target-feature=+crt-static",
                "opt-level=2",
            ] {
                cmd = cmd.arg("-C").arg(flag);
            }
            cmd = cmd.arg("-C").arg(format!("linker={RUST_LD}"));
            cmd = cmd
                .arg("-o")
                .arg(format!("/tools/{name}"))
                .arg(src_host.to_string_lossy().into_owned())
                .current_dir(std::env::temp_dir().to_string_lossy());

            let result = cmd.run(ctx).await;
            let _ = std::fs::remove_file(&src_host);

            if result.is_error {
                let detail = result.content.as_text();
                return ToolResult::err(format!(
                    "rustc failed:\n{detail}\n\nCompile errors above; if the \
                     source needs C libraries, the sandbox does not yet ship a \
                     musl C cross toolchain (spec §16 note)."
                ));
            }

            // Verify the binary landed + is executable on the host side.
            if !out_host.is_file() {
                return ToolResult::err(format!(
                    "compile reported success but {} was not produced in {} \
                     (check the /tools bind)",
                    name,
                    tools_host.display()
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&out_host, std::fs::Permissions::from_mode(0o755));
            }

            ToolResult::ok_str(format!(
                "Compiled static musl tool `{name}` → {} ({PROTOCOL_HINT})",
                out_host.display()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn userns_available() -> bool {
        std::process::Command::new("unshare")
            .args(["-Ur", "true"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn tmp(dir: &str, tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ri-rustc-{tag}-{dir}-{}", std::process::id()))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rustc_tool_bootstraps_static_tool_into_tools_dir() {
        use crate::container::rootfs::assemble_image;

        if !userns_available() {
            eprintln!("SKIP: unprivileged user namespaces unavailable");
            return;
        }
        if !toolchain_candidates().iter().any(|d| toolchain_valid(d)) {
            eprintln!("SKIP: no Rust toolchain provisioned (fetch-rust-toolchain.sh)");
            return;
        }
        let _g = crate::container::SANDBOX_ENV_LOCK.lock().await;

        let img = tmp("img", "boot");
        let home = tmp("home", "boot");
        std::fs::remove_dir_all(&img).ok();
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&img).ok();
        std::fs::create_dir_all(home.join(".ri").join("tools")).unwrap();
        let tools_bin = home.join(".ri").join("tools").join("greet_tool");

        // Point image/toolchain/home at scratch + real assets for the duration
        // (edition-2024 unsafe; serialized by ENV_LOCK).
        unsafe {
            std::env::set_var("RI_SANDBOX_IMAGE", &img);
            std::env::set_var("RI_SANDBOX_TOOLCHAIN", toolchain_candidates()[0].clone());
            std::env::set_var("HOME", &home);
        }
        assemble_image(&img).expect("assemble scratch image");

        let source = r#"
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--describe") {
        println!("{}", "{\"name\":\"greet_tool\",\"description\":\"greets\",\"parameters\":{\"type\":\"object\",\"properties\":{}}}");
        return;
    }
    let who = args.iter().skip(1).next().map(|s| s.as_str()).unwrap_or("nobody");
    println!("GREET-OK:{}", who);
}
"#;

        let ctx = ToolCallContext {
            id: "rustc-boot".to_string(),
            tx: None,
            cancel_rx: None,
            subagent: None,
            root: Some(home.clone()),
            sandbox: true,
        };
        let result = RustcTool
            .run(json!({"source": source, "name": "greet_tool"}), ctx)
            .await;
        let text = result.content.as_text().to_string();
        assert!(!result.is_error, "rustc bootstrap failed: {text}");
        assert!(
            tools_bin.is_file(),
            "compiled tool must land in ~/.ri/tools"
        );

        // Run the freshly compiled static tool inside the sandbox.
        let run_ctx = ToolCallContext {
            id: "rustc-run".to_string(),
            tx: None,
            cancel_rx: None,
            subagent: None,
            root: Some(home.clone()),
            sandbox: true,
        };
        let out = crate::agent::tools::subprocess::SubprocessCommand::new("/tools/greet_tool")
            .arg("world")
            .sandboxed(true)
            .run(run_ctx)
            .await;
        let out_text = out.content.as_text().to_string();
        assert!(
            !out.is_error && out_text.contains("GREET-OK:world"),
            "compiled static tool must run in the sandbox: {out_text}"
        );

        unsafe {
            std::env::remove_var("RI_SANDBOX_IMAGE");
            std::env::remove_var("RI_SANDBOX_TOOLCHAIN");
            std::env::remove_var("HOME");
        }
        std::fs::remove_dir_all(&img).ok();
        std::fs::remove_dir_all(&home).ok();
    }
}
