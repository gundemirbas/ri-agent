use std::{
    collections::HashSet,
    env,
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    sync::{Arc, LazyLock, Mutex},
};

use serde_json::Value;

use super::subprocess::SubprocessCommand;
use crate::agent::types::{Tool, ToolCallContext, ToolResult};

// ── CustomTool ────────────────────────────────────────────────────────────────

/// A user-defined tool loaded from an executable on disk.
///
/// The executable must implement the describe/invoke protocol:
/// - `executable --describe` → JSON descriptor on stdout
/// - UTF-8 JSON on stdin → UTF-8 result on stdout; non-zero exit = error
///
/// Rich or structured write parameters must document and prefer a UTF-8
/// `--patch-file`, `--fields-file`, or stdin interface.
pub struct CustomTool {
    /// Absolute path to the executable.
    path: PathBuf,
    name: String,
    description: String,
    schema: Value,
}

impl Tool for CustomTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn run(
        &self,
        args: Value,
        ctx: ToolCallContext,
    ) -> Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args_json = args.to_string();

            SubprocessCommand::new(self.path.to_string_lossy())
                .stdin_data(args_json.into_bytes())
                .error_on_nonzero_exit()
                .sandboxed(ctx.sandbox)
                .run(ctx)
                .await
        })
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Returns the ordered list of directories to search for custom tools:
/// 1. `~/.ri/tools/`
/// 2. `./.ri/tools/` (project-local)
/// 3. `ProjectDirs::config_dir()/tools/`
pub fn custom_tool_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(home) = env::var_os("HOME").filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(home).join(".ri").join("tools"));
    }

    if let Ok(cwd) = env::current_dir() {
        dirs.push(cwd.join(".ri").join("tools"));
    }

    if let Ok(proj) = crate::dirs::project_dirs() {
        dirs.push(proj.config_dir().join("tools"));
    }

    dirs
}

/// Scan `roots` for executable files, run `executable --describe` on each,
/// parse the JSON descriptor, and return the resulting [`CustomTool`] list.
///
/// Roots are deduplicated by canonical path. Files that are not executable,
/// fail to run, or return invalid JSON are silently skipped (logged at debug).
///
/// The returned tools are in directory-traversal order (sorted by name within
/// each directory).
pub fn load_custom_tools(roots: &[PathBuf]) -> Vec<CustomTool> {
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut tools = Vec::new();

    for root in roots {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        if !seen_dirs.insert(canonical) {
            continue;
        }
        if !root.is_dir() {
            continue;
        }
        tools.extend(load_tools_from_dir(root));
    }

    tools
}

// ── Hot reload ───────────────────────────────────────────────────────────────

/// Custom-tool names this process has registered into some [`ToolRegistry`].
/// Lets [`refresh_custom_tools`] tell "ours" apart from built-ins: only names
/// tracked here are ever dropped or overwritten during a refresh.
static CUSTOM_NAMES: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Record a custom-tool name as owned by us (called on initial registration
/// so a later [`refresh_custom_tools`] can update/drop it in place).
pub fn track_custom(name: &str) {
    CUSTOM_NAMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string());
}

/// Rescan the custom-tool dirs and merge the result into `registry` in place.
///
/// Purpose: the `rustc`/`muslcc` tools compile new binaries into the
/// custom-tool dirs at runtime; this lets the agent call them a turn later
/// without restarting the app. Rules:
/// - built-in tools are never touched (a custom tool whose name collides with
///   a built-in is skipped, like at initial registration),
/// - custom tools that disappeared from disk are dropped,
/// - custom tools still present are refreshed in place.
///
/// Returns the number of custom tools currently registered.
pub fn refresh_custom_tools(registry: &mut super::super::types::ToolRegistry) -> usize {
    let fresh = load_custom_tools(&custom_tool_dirs());
    let fresh_names: HashSet<String> = fresh.iter().map(|t| t.name().to_string()).collect();

    let mut ours = CUSTOM_NAMES.lock().unwrap_or_else(|e| e.into_inner());

    // Drop tracked tools that no longer exist on disk.
    let stale: Vec<String> = ours
        .iter()
        .filter(|n| !fresh_names.contains(*n))
        .cloned()
        .collect();
    for name in &stale {
        registry.remove(name);
        ours.remove(name);
    }

    // Insert or refresh the current set.
    for tool in fresh {
        let name = tool.name().to_string();
        if ours.contains(&name) {
            // Ours already — refresh the implementation in place.
            registry.insert(name, Arc::new(tool));
        } else if registry.contains_key(&name) {
            // A built-in (or third-party) tool owns this name — skip.
            log::debug!(
                "custom tool '{name}' skipped (refresh): name conflicts with a built-in tool"
            );
        } else {
            registry.insert(name.clone(), Arc::new(tool));
            ours.insert(name);
        }
    }

    ours.len()
}

fn load_tools_from_dir(dir: &std::path::Path) -> Vec<CustomTool> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_executable(p))
        .collect();

    paths.sort();

    paths
        .into_iter()
        .filter_map(|path| load_tool_from_executable(&path))
        .collect()
}

/// Run `executable --describe` synchronously and parse the JSON descriptor.
/// Returns `None` (and logs at debug) if anything goes wrong.
fn load_tool_from_executable(path: &std::path::Path) -> Option<CustomTool> {
    // Retry once on ETXTBSY: another thread may have a write fd open on the
    // same inode (e.g. a NamedTempFile in a concurrent test) for a very brief
    // window.  A short sleep is always sufficient to outlast it.
    let output = {
        let attempt = std::process::Command::new(path)
            .arg("--describe")
            .stdin(Stdio::null())
            .output();
        match attempt {
            Err(ref e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                std::process::Command::new(path)
                    .arg("--describe")
                    .stdin(Stdio::null())
                    .output()
            }
            other => other,
        }
    };

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            log::debug!(
                "custom tool: failed to run --describe on {}: {e}",
                path.display()
            );
            return None;
        }
    };

    if !output.status.success() {
        log::debug!(
            "custom tool: --describe exited with {} for {}",
            output.status,
            path.display()
        );
        return None;
    }

    let json: Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(e) => {
            log::debug!(
                "custom tool: invalid JSON from --describe on {}: {e}",
                path.display()
            );
            return None;
        }
    };

    let name = json.get("name").and_then(Value::as_str)?.trim().to_string();
    let description = json
        .get("description")
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    let schema = json
        .get("parameters_schema")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));

    if name.is_empty() || description.is_empty() {
        log::debug!(
            "custom tool: missing name or description from --describe on {}",
            path.display()
        );
        return None;
    }

    Some(CustomTool {
        path: path.to_path_buf(),
        name,
        description,
        schema,
    })
}

// ── Platform helpers ──────────────────────────────────────────────────────────

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Write a shell script to `path` with the execute bit set.
    fn write_script(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn describe_script(name: &str, description: &str) -> String {
        format!(
            r#"#!/bin/sh
if [ "$1" = "--describe" ]; then
  printf '{{"name":"{name}","description":"{description}","parameters_schema":{{"type":"object","properties":{{"input":{{"type":"string"}}}}}}}}'
  exit 0
fi
input=$(cat)
printf "got: $input"
"#
        )
    }

    #[test]
    fn loads_valid_tool_from_directory() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("my_tool");
        write_script(&script_path, &describe_script("my_tool", "Does something."));

        let tools = load_custom_tools(&[dir.path().to_path_buf()]);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "my_tool");
        assert_eq!(tools[0].description(), "Does something.");
    }

    #[test]
    fn skips_invalid_json_from_describe() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("bad_tool");
        write_script(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"--describe\" ]; then echo 'not json'; fi\n",
        );

        let tools = load_custom_tools(&[dir.path().to_path_buf()]);
        assert!(tools.is_empty());
    }

    #[test]
    fn skips_nonzero_describe_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("fail_tool");
        write_script(
            &script_path,
            "#!/bin/sh\nif [ \"$1\" = \"--describe\" ]; then exit 1; fi\n",
        );

        let tools = load_custom_tools(&[dir.path().to_path_buf()]);
        assert!(tools.is_empty());
    }

    #[test]
    fn skips_non_executable_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not_executable");
        // Write file without execute bit.
        std::fs::write(&path, "#!/bin/sh\necho hello\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();

        let tools = load_custom_tools(&[dir.path().to_path_buf()]);
        assert!(tools.is_empty());
    }

    #[test]
    fn deduplicates_same_directory_via_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("my_tool");
        write_script(&script_path, &describe_script("my_tool", "Desc."));

        // Pass the same directory twice (once as canonical, once as raw).
        let roots = vec![dir.path().to_path_buf(), dir.path().to_path_buf()];
        let tools = load_custom_tools(&roots);
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn empty_directory_returns_no_tools() {
        let dir = tempfile::tempdir().unwrap();
        let tools = load_custom_tools(&[dir.path().to_path_buf()]);
        assert!(tools.is_empty());
    }

    #[test]
    fn nonexistent_directory_returns_no_tools() {
        let tools = load_custom_tools(&[PathBuf::from("/nonexistent/ri/tools")]);
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn execute_passes_args_on_stdin_and_returns_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("echo_tool");
        write_script(&script_path, &describe_script("echo_tool", "Echoes input."));

        let tools = load_custom_tools(&[dir.path().to_path_buf()]);
        assert_eq!(tools.len(), 1);

        let result = tools[0]
            .execute(serde_json::json!({"input": "hello"}))
            .await;
        assert!(!result.is_error);
        assert!(
            result.content.as_text().contains("got:"),
            "got: {}",
            result.content.as_text()
        );
    }

    #[tokio::test]
    async fn execute_nonzero_exit_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("fail_on_run");
        write_script(
            &script_path,
            r#"#!/bin/sh
if [ "$1" = "--describe" ]; then
  printf '{"name":"fail_on_run","description":"Always fails.","parameters_schema":{"type":"object","properties":{}}}'
  exit 0
fi
cat > /dev/null
echo "something went wrong"
exit 1
"#,
        );

        let tools = load_custom_tools(&[dir.path().to_path_buf()]);
        assert_eq!(tools.len(), 1);

        let result = tools[0].execute(serde_json::json!({})).await;
        assert!(
            result.is_error,
            "expected is_error, got: {:?}",
            result.content.as_text()
        );
        assert!(
            result.content.as_text().contains("exit 1"),
            "expected 'exit 1' in content, got: {:?}",
            result.content.as_text()
        );
    }

    #[test]
    fn describe_missing_name_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("no_name");
        write_script(
            &script_path,
            r#"#!/bin/sh
if [ "$1" = "--describe" ]; then
  printf '{"description":"No name here.","parameters_schema":{"type":"object","properties":{}}}'
  exit 0
fi
"#,
        );

        let tools = load_custom_tools(&[dir.path().to_path_buf()]);
        assert!(tools.is_empty());
    }
}

#[cfg(test)]
mod hot_reload_tests {
    use super::*;
    use crate::agent::types::ToolRegistry;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex as StdMutex;

    /// The custom dirs follow `$HOME`; these sync tests serialize a HOME
    /// override among themselves (a plain Mutex is fine here — only these
    /// tests touch HOME in the lib test process).
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn unique(prefix: &str) -> String {
        format!("ri_hotrl_{prefix}_{}", std::process::id())
    }

    fn write_tool(dir: &std::path::Path, name: &str) {
        let path = dir.join(name);
        let body = format!(
            r#"#!/bin/sh
if [ "$1" = "--describe" ]; then
  printf '{{"name":"{name}","description":"hot-reload fixture.","parameters_schema":{{"type":"object","properties":{{}}}}}}'
  exit 0
fi
printf '%s' "ran:{name}"
"#
        );
        std::fs::write(&path, body).unwrap();
        let mut p = std::fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&path, p).unwrap();
    }

    #[test]
    fn refresh_adds_updates_and_drops_custom_tools() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(unique("home"));
        let tools = home.join(".ri").join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        unsafe { std::env::set_var("HOME", &home) };

        let a = unique("a");
        let b = unique("b");
        write_tool(&tools, &a);

        let mut registry: ToolRegistry = ToolRegistry::new();
        assert_eq!(refresh_custom_tools(&mut registry), 1);
        assert!(registry.contains_key(&a), "new tool must be added: {a}");

        // A tool compiled later (e.g. by rustc) becomes callable on refresh.
        write_tool(&tools, &b);
        assert_eq!(refresh_custom_tools(&mut registry), 2);
        assert!(
            registry.contains_key(&b),
            "newly written tool must appear: {b}"
        );

        // A tool deleted from disk is dropped from the registry.
        std::fs::remove_file(tools.join(&a)).unwrap();
        assert_eq!(refresh_custom_tools(&mut registry), 1);
        assert!(
            !registry.contains_key(&a),
            "deleted custom tool must be dropped"
        );
        assert!(registry.contains_key(&b));

        std::fs::remove_dir_all(&home).ok();
        unsafe { std::env::remove_var("HOME") };
    }

    /// A name that collides with an existing (built-in) tool is skipped; the
    /// built-in implementation must survive untouched.
    #[test]
    fn refresh_never_overwrites_builtin_names() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(unique("home2"));
        let tools = home.join(".ri").join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        unsafe { std::env::set_var("HOME", &home) };

        let clash = unique("clash");
        write_tool(&tools, &clash);

        // Fake built-in occupying the same name.
        struct Fake {
            n: String,
        }
        impl Tool for Fake {
            fn name(&self) -> &str {
                &self.n
            }
            fn description(&self) -> &str {
                "builtin"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            fn run(
                &self,
                _args: serde_json::Value,
                _ctx: ToolCallContext,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>>
            {
                Box::pin(async { ToolResult::ok_str("builtin-ran") })
            }
        }
        let mut registry: ToolRegistry = ToolRegistry::new();
        registry.insert(
            clash.clone(),
            std::sync::Arc::new(Fake { n: clash.clone() }),
        );

        // The colliding custom tool never becomes "ours" (nothing tracked), and
        // the built-in must still win the name; the custom executable is skipped.
        assert_eq!(
            refresh_custom_tools(&mut registry),
            0,
            "collision is not tracked"
        );
        assert_eq!(registry.len(), 1);
        let tool = registry.get(&clash).expect("name still present");
        assert_eq!(
            tool.description(),
            "builtin",
            "built-in must not be replaced"
        );

        std::fs::remove_dir_all(&home).ok();
        unsafe { std::env::remove_var("HOME") };
    }
}
