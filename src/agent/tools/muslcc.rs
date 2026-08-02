//! The `muslcc` tool — compile C inside the sandbox (bootstrapping, C side).
//!
//! The sister tool to `rustc`: the agent supplies a **single-file C source**;
//! this tool compiles it **inside the sandbox** with the bundled musl C cross
//! compiler and installs the resulting static binary into the host
//! custom-tools directory (`~/.ri/tools`).
//!
//! Compiler: musl.cc `x86_64-linux-musl-cross` (fetched by
//! `scripts/fetch-rust-toolchain.sh` into `rootfs/toolchain/musl-cross`, bound
//! into the sandbox at its canonical `/x86_64-linux-musl-cross` prefix). The
//! driver is a fully static i386 binary and relocatable, so it boots inside
//! the strict musl-only image with **no host musl-dev and no extra libs** —
//! the cross toolchain carries its own musl and produces a static-pie binary
//! with `-static`. C sources that merely include `<stdio.h>`/`<stdlib.h>` etc.
//! build out of the box; third-party C libraries must be vendored alongside
//! (see `docs/CONTAINER-RUNTIME-SPEC.md` §16).

use std::pin::Pin;

use serde_json::Value;

use super::custom::custom_tool_dirs;
use super::subprocess::SubprocessCommand;
use crate::agent::types::{Tool, ToolCallContext, ToolResult};
use crate::container::rootfs::{toolchain_candidates, toolchain_valid};

/// Sandbox path of the cross gcc (bound into the image at the canonical musl.cc
/// prefix so its relocatable sysroot resolution finds `<prefix>/x86_64-linux-musl`).
pub const CROSS_GCC: &str = "/x86_64-linux-musl-cross/bin/x86_64-linux-musl-gcc";

/// The cross gcc sub-directory inside each (potentially valid) toolchain dir.
const CROSS_REL: &str = "musl-cross/bin/x86_64-linux-musl-gcc";

/// Returns true when a provisioned toolchain ships the musl C cross compiler.
pub fn musl_cross_provisioned() -> bool {
    toolchain_candidates()
        .iter()
        .filter(|d| toolchain_valid(d))
        .any(|d| d.join(CROSS_REL).is_file())
}

#[derive(serde::Deserialize)]
struct MuslccArgs {
    /// The complete C source of the tool (single translation unit).
    source: String,
    /// Output tool name (filename inside the tools dir). Optional.
    #[serde(default)]
    name: Option<String>,
}

pub struct MuslccTool;

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
        "ri_c_tool".to_string()
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

impl Tool for MuslccTool {
    fn name(&self) -> &str {
        "muslcc"
    }

    fn description(&self) -> &str {
        "Compile a single-file C program into a static musl custom tool inside \
         the sandbox and install it in the custom-tools directory (~/.ri/tools). \
         The tool then follows the custom-tool protocol: `--describe` prints a \
         JSON descriptor; JSON on stdin returns a result on stdout. Requires the \
         sandbox (--sandbox) and the provisioned musl C cross toolchain."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Complete C11 source of the tool (single translation unit; stdio/stdlib ok). Implement main() that parses an optional --describe flag and JSON args on stdin."
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
                    "the `muslcc` bootstrap tool compiles inside the sandbox; \
                     start ri with --sandbox (or sandbox = true in config.toml)",
                );
            }
            let parsed: MuslccArgs = match serde_json::from_value(args) {
                Ok(p) => p,
                Err(e) => return ToolResult::err(format!("bad arguments: {e}")),
            };

            // The cross compiler must be provisioned (fetched together with the
            // Rust toolchain by scripts/fetch-rust-toolchain.sh).
            if !musl_cross_provisioned() {
                return ToolResult::err(
                    "no musl C cross toolchain provisioned: run \
                     `scripts/fetch-rust-toolchain.sh` (it also fetches \
                     musl.cc x86_64-linux-musl-cross into rootfs/toolchain; \
                     see docs/CONTAINER-RUNTIME-SPEC.md §16)",
                );
            }

            let name = sanitize_name(parsed.name.as_deref().unwrap_or("ri_c_tool"));

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
                std::env::temp_dir().join(format!("ri-muslcc-{}-{name}.c", std::process::id()));
            if let Err(e) = std::fs::write(&src_host, parsed.source.as_bytes()) {
                return ToolResult::err(format!("cannot write source: {e}"));
            }

            let cmd = SubprocessCommand::new(CROSS_GCC)
                .arg("-static")
                .arg("-O2")
                .arg("-o")
                .arg(format!("/tools/{name}"))
                .arg(src_host.to_string_lossy().into_owned())
                .current_dir(std::env::temp_dir().to_string_lossy())
                .sandboxed(ctx.sandbox);

            let result = cmd.run(ctx).await;
            let _ = std::fs::remove_file(&src_host);

            if result.is_error {
                let detail = result.content.as_text();
                return ToolResult::err(format!(
                    "muslcc failed:\n{detail}\n\nThe cross compiler carries its \
                     own musl (no host musl-dev needed); if you need a \
                     third-party C library, vendor it next to the source."
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
        std::env::temp_dir().join(format!("ri-muslcc-{tag}-{dir}-{}", std::process::id()))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn muslcc_tool_bootstraps_static_c_tool_into_tools_dir() {
        use crate::container::rootfs::assemble_image;

        if !userns_available() {
            eprintln!("SKIP: unprivileged user namespaces unavailable");
            return;
        }
        if !musl_cross_provisioned() {
            eprintln!(
                "SKIP: no musl C cross toolchain provisioned \
                 (scripts/fetch-rust-toolchain.sh)"
            );
            return;
        }
        let _g = crate::container::SANDBOX_ENV_LOCK.lock().await;

        let img = tmp("img", "boot");
        let home = tmp("home", "boot");
        std::fs::remove_dir_all(&img).ok();
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&img).ok();
        std::fs::create_dir_all(home.join(".ri").join("tools")).unwrap();
        let tools_bin = home.join(".ri").join("tools").join("greet_c");

        // Point image/toolchain/home at scratch + real assets for the duration
        // (edition-2024 unsafe; serialized by SANDBOX_ENV_LOCK).
        unsafe {
            std::env::set_var("RI_SANDBOX_IMAGE", &img);
            std::env::set_var("RI_SANDBOX_TOOLCHAIN", toolchain_candidates()[0].clone());
            std::env::set_var("HOME", &home);
        }
        assemble_image(&img).expect("assemble scratch image");

        let source = r#"
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "--describe") == 0) {
        printf("{\"name\":\"greet_c\",\"description\":\"greets from C\",\"parameters_schema\":{\"type\":\"object\",\"properties\":{}}}");
        return 0;
    }
    char who[128] = "nobody";
    if (argc > 1) { snprintf(who, sizeof who, "%s", argv[1]); }
    printf("GREET-C:%s\n", who);
    return 0;
}
"#;

        let ctx = ToolCallContext {
            id: "muslcc-boot".to_string(),
            tx: None,
            cancel_rx: None,
            subagent: None,
            root: Some(home.clone()),
            sandbox: true,
        };
        let result = MuslccTool
            .run(json!({"source": source, "name": "greet_c"}), ctx)
            .await;
        let text = result.content.as_text().to_string();
        assert!(!result.is_error, "muslcc bootstrap failed: {text}");
        assert!(
            tools_bin.is_file(),
            "compiled tool must land in ~/.ri/tools"
        );

        // Run the freshly compiled static C tool inside the sandbox.
        let run_ctx = ToolCallContext {
            id: "muslcc-run".to_string(),
            tx: None,
            cancel_rx: None,
            subagent: None,
            root: Some(home.clone()),
            sandbox: true,
        };
        let out = crate::agent::tools::subprocess::SubprocessCommand::new("/tools/greet_c")
            .arg("world")
            .sandboxed(true)
            .run(run_ctx)
            .await;
        let out_text = out.content.as_text().to_string();
        assert!(
            !out.is_error && out_text.contains("GREET-C:world"),
            "compiled static C tool must run in the sandbox: {out_text}"
        );

        // Hot-reload: the fresh binary must be visible to a registry refresh
        // and runnable through the custom-tool protocol (--describe).
        let mut registry = crate::agent::types::ToolRegistry::new();
        crate::agent::tools::custom::refresh_custom_tools(&mut registry);
        assert!(
            registry.contains_key("greet_c"),
            "compiled C tool must hot-reload into the registry"
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
