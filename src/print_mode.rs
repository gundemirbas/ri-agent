//! Non-interactive (headless / `--print`) mode support.
//!
//! Handles the agent loop when ri is invoked with `--print` / `-p`, streaming
//! the response to stdout and exiting.

use std::io::{self, ErrorKind, Write};
use std::sync::Arc;

use crate::agent::AgentLoopConfig;
use crate::agent::tools::custom::{custom_tool_dirs, load_custom_tools};
use crate::agent::tools::register_builtin_tools;
use crate::agent::types::CancelLevel;
use crate::agent::{AgentEvent, ToolOutputLog, build_system_prompt};
use crate::app_event::AppEvent;
use crate::llm;
use crate::provider::build_provider_for_instance;
use crate::provider_instance::ProviderInstance;
use crate::provider_manager::format_provider_error_for_display;
use crate::provider_setup::{
    resolve_provider_instance, resolve_thinking_level_for_model, with_resolved_model,
};
use crate::skills;
use crate::tool_presentation;

use super::build_file_tracker;

// ── Shared helpers ────────────────────────────────────────────────────────

pub(crate) fn provider_display_name(instance: &ProviderInstance) -> String {
    instance.backend_preset.label().to_string()
}

// ── Main entry ────────────────────────────────────────────────────────────

pub(crate) async fn run_print_mode(
    prompt: String,
    provider_override: &str,
    model_override: Option<&str>,
    config: &crate::config::RiConfig,
) -> io::Result<()> {
    let resolved_instance = with_resolved_model(
        model_override,
        &resolve_provider_instance(Some(provider_override), config)
            .map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?,
    );
    let current_thinking =
        resolve_thinking_level_for_model(config, resolved_instance.effective_model());

    let provider = build_provider_for_instance(&resolved_instance, current_thinking, config)
        .map_err(|e| io::Error::other(format!("provider error: {e}")))?;

    let custom_tools = load_custom_tools(&custom_tool_dirs());
    let headless_tracker = Arc::new(std::sync::Mutex::new(build_file_tracker()));
    let loaded_skills = Arc::new(skills::load_skills());
    let tools = register_builtin_tools(
        None,
        Arc::clone(&headless_tracker),
        Arc::clone(&loaded_skills),
        custom_tools,
    )
    .await;
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let headless_log = Arc::new(std::sync::Mutex::new(ToolOutputLog::new("headless")));
    let system_prompt = build_system_prompt(&tools, &cwd, &loaded_skills, None);

    let session_events = vec![crate::session_event::SessionEvent::UserMessage {
        content: prompt.clone(),
        timestamp: crate::app_agent_handlers::now_ts(),
    }];

    let loop_config = AgentLoopConfig {
        tools: tools.clone(),
        file_tracker: headless_tracker,
        tool_output_log: headless_log,
        session_events,
        current_model: resolved_instance.effective_model().to_string(),
        auto_compaction_enabled: true,
        manual_compaction_instructions: None,
        executor: std::sync::Arc::new({
            let mut ex = crate::agent::DefaultToolExecutor::new();
            // Wire subagent launching in headless mode too: subagents can
            // delegate against the same provider/tool universe.
            ex.subagent = Some(crate::agent::types::SubagentContext {
                provider: Arc::clone(&provider),
                agents: std::sync::Arc::new(crate::agents::load_agents()),
                skills: std::sync::Arc::new((*loaded_skills).clone()),
                cwd: cwd.clone(),
                tools: tools.clone(),
            });
            ex
        }),
        system_prompt: Some(system_prompt),
    };

    let exit_code = run_print_mode_loop(
        loop_config,
        &provider_display_name(&resolved_instance),
        provider,
    )
    .await;

    std::process::exit(exit_code);
}

// ── Agent event loop ─────────────────────────────────────────────────────

/// Drive the agent event loop for `--print` mode. Returns the process exit code.
async fn run_print_mode_loop(
    config: AgentLoopConfig,
    provider_label: &str,
    provider: std::sync::Arc<dyn llm::LlmProvider + Send + Sync>,
) -> i32 {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let (_steering_tx, steering_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(CancelLevel::None);

    tokio::spawn(async move {
        crate::agent::run_agent_loop(config, provider, tx, steering_rx, cancel_rx).await;
    });

    while let Some(ev) = rx.recv().await {
        let AppEvent::Agent(ev) = ev else {
            continue;
        };
        match ev {
            AgentEvent::TextToken { text, .. } => {
                print!("{text}");
                let _ = io::stdout().flush();
            }
            AgentEvent::ThinkingToken(_) | AgentEvent::Usage(_) => {
                // Suppress thinking tokens and usage in print mode.
            }
            AgentEvent::ToolCallIntent { .. }
            | AgentEvent::ToolCallArgsDelta { .. }
            | AgentEvent::SteeringConsumed { .. } => {
                // No-op in print mode.
            }
            AgentEvent::StatusUpdate(msg) => {
                eprintln!("{msg}");
            }
            AgentEvent::Compacting => {
                eprintln!("compacting…");
            }
            AgentEvent::CompactionDone(outcome) => {
                eprintln!(
                    "compacted: {}k → {}k tokens",
                    outcome.tokens_before / 1000,
                    outcome.tokens_after / 1000
                );
            }
            AgentEvent::ToolCallStart { name, args, .. } => {
                let (label, _) = tool_presentation::tool_invocation_label(
                    &name,
                    &args,
                    None,
                    &crate::config::DisplayConfig::default(),
                );
                eprintln!("{label}");
            }
            AgentEvent::ToolCallEnd { result, .. } => {
                if result.is_error {
                    eprintln!(
                        "  ✗ {}",
                        result.content.as_text().lines().next().unwrap_or("error")
                    );
                }
            }
            AgentEvent::ToolOutputChunk { .. } => {}
            AgentEvent::TurnEnd => {}
            AgentEvent::ExternalFileChange { paths, .. } => {
                for path in &paths {
                    eprintln!("⚠️  {} was modified externally", path.display());
                }
            }
            AgentEvent::Done => {
                println!(); // final newline after streamed output
                return 0;
            }
            AgentEvent::Error(e) => {
                let rendered = format_provider_error_for_display(provider_label, &e);
                eprintln!("error: {rendered}");
                return 1;
            }
        }
    }

    0
}
