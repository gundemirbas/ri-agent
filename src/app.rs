use ratatui_textarea::{CursorMove, TextArea};
use std::sync::Arc;

use crate::{
    agent::AgentLoopConfig,
    app_event::{AppEvent, AppEventTx},
    completion::{self, CompletionItem},
    config::DisplayConfig,
    keybindings::{BindingContext, KEYBINDINGS},
    live_turn::compose_display,
    llm::{LlmProvider, Message, UsageStats},
    provider_instance::{ApiType, ProviderInstance},
    session::SessionStore,
    session_state::SessionState,
    skills::SkillMeta,
    theme::Theme,
    thinking::ThinkingLevel,
};

use crate::agent_runtime::AgentRuntime;
use crate::agent_turn_state::AgentTurnState;
use crate::ask_user_state::AskUserState;
use crate::completion_state::CompletionState;
use crate::log_view_state::LogViewState;
use crate::mouse_select::MouseSelectState;
use crate::provider_manager::{
    ProviderManager, ProviderSetupStep, active_provider_display_name,
    format_provider_error_for_display,
};
use crate::selection_state::{SelectionKind, SelectionState};
use crate::session_event::SessionEvent;
use crate::session_manager::SessionManager;
use crate::shell_state::ShellState;
use crate::step_back_state::StepBackState;
use crate::tracked::Tracked;

// ── Streaming status ──────────────────────────────────────────────────────────

/// Describes what the agent/provider is currently doing while a turn is active.
#[derive(Debug, Clone)]
pub enum StreamingStatus {
    /// Waiting for the first token — throbber should animate.
    Waiting,
    /// Provider-supplied transient message (e.g. rate-limit countdown).
    Message(String),
    /// A completed-turn status message that remains visible until the next turn starts.
    CompletedMessage(String),
}

// ── Selection result ──────────────────────────────────────────────────────────

/// Value returned when the user confirms a choice in the selection menu.
pub enum SelectionResult {
    Model(String),
    Thinking(ThinkingLevel),
    Provider(String),
    ResumeSession(String),
    AskOption(String),
    AskFreeform,
    /// The user cancelled a pending provider removal confirmation.
    CancelProviderRemoval,
    /// The user confirmed removing a custom provider instance.
    RemoveProvider(String),
    /// The user selected an agent name from the picker.
    Agent(String),
    /// The user chose to add a new OpenAI-compatible provider instance.
    ProviderAdd,
    /// An API type was chosen during add-provider setup.
    ProviderApiType(ApiType),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Chat,
    Shell,
}

// ── Login state ───────────────────────────────────────────────────────────────
// ── Log cache ─────────────────────────────────────────────────────────────────

// ── App state ─────────────────────────────────────────────────────────────────

pub struct App {
    pub(crate) textarea: TextArea<'static>,
    /// Shell mode state (textarea, selected shell, available shells).
    pub(crate) shell: ShellState,
    pub(crate) input_mode: InputMode,
    /// Vertical scroll offset for the input panel (in wrapped lines).
    pub(crate) input_scroll: usize,
    /// Log pane scroll and cache state.
    pub(crate) log_view: LogViewState,
    /// Active agent turn state: streaming status, throbber tick, last output time.
    pub(crate) agent_turn: AgentTurnState,
    /// All provider-related state: instances, active instance/model/thinking,
    /// and transient setup-flow state.
    pub(crate) provider: ProviderManager,
    /// Agent loop configuration (tools).
    pub(crate) agent_config: AgentLoopConfig,
    /// Skills loaded from all supported skill roots.
    pub(crate) loaded_skills: Vec<SkillMeta>,
    /// User-definable agent profiles loaded from filesystem.
    pub(crate) agents: Vec<crate::agents::AgentMeta>,
    /// Name of the currently active agent, or `None` for the default.
    pub(crate) active_agent: Option<String>,

    // ── Completion popup + model fetch ────────────────────────────────────────
    pub(crate) completion: CompletionState,

    // ── Generic selection menu ────────────────────────────────────────────────
    pub(crate) selection: SelectionState,

    // ── Info bar ──────────────────────────────────────────────────────────────
    /// When true, the info bar (provider / model / context window) is shown
    /// below the input panel.  Toggled by Ctrl+I.
    pub(crate) show_info: bool,
    /// Best-effort token usage reported for the latest completed turn.
    pub(crate) latest_usage: Option<UsageStats>,
    /// Set when the previous turn should have populated the prompt cache but
    /// the current response shows zero cached tokens.  Cleared when the user
    /// submits a new message.
    pub(crate) cache_miss_warning: bool,

    // ── Session persistence + state ───────────────────────────────────────────
    /// All session-related state: persistence store, committed state, live
    /// turn overlay, and pending event buffer.
    pub(crate) session: Tracked<SessionManager>,

    // ── Ask-user interaction state ──────────────────────────────────────────
    pub(crate) ask_user: AskUserState,

    // ── Runtime/task state ───────────────────────────────────────────────────
    pub(crate) runtime: AgentRuntime,

    // ── Agent execution backend (Local vs ACP) ───────────────────────────────
    pub(crate) backend: AgentBackend,

    // ── Step-back state ──────────────────────────────────────────────────────
    pub(crate) step_back: StepBackState,

    // ── Theme ─────────────────────────────────────────────────────────────────
    pub(crate) theme: Theme,
    // ── Display thresholds ────────────────────────────────────────────────────
    pub(crate) display: DisplayConfig,

    // ── Rootless sandbox for tool subprocesses ───────────────────────────────
    /// When `true`, the in-process agent routes tool subprocesses through the
    /// `ri-sandbox` container child (user namespace + chroot). Wired by `main`
    /// from `--sandbox` / config.toml `sandbox = true`.
    pub(crate) sandbox: bool,

    // ── Mouse selection ───────────────────────────────────────────────────────
    pub(crate) mouse_select: MouseSelectState,
}

// Convenience alias used throughout this module.
pub(crate) type DynProvider = Arc<dyn LlmProvider + Send + Sync + 'static>;

/// How the agent turn is executed: in-process (`Local`, the default) or over
/// the Agent Client Protocol by a detached `ri --serve` child (`--tui-acp`).
pub(crate) enum AgentBackend {
    Local,
    Acp(crate::acp_tui::AcpTuiControls),
}

impl App {
    pub fn new(
        initial_instance: ProviderInstance,
        initial_model: impl Into<String>,
        initial_thinking: ThinkingLevel,
        agent_config: AgentLoopConfig,
        display: DisplayConfig,
    ) -> Self {
        let initial_model = initial_model.into();
        Self {
            textarea: Self::make_textarea(),
            shell: ShellState::new(),
            input_mode: InputMode::Chat,
            input_scroll: 0,
            log_view: LogViewState::new(),
            agent_turn: AgentTurnState::new(),
            provider: ProviderManager::new(initial_instance, initial_model, initial_thinking),
            agent_config,
            loaded_skills: Vec::new(),
            agents: Vec::new(),
            active_agent: None,
            completion: CompletionState::new(),
            selection: SelectionState::new(),
            show_info: false,
            latest_usage: None,
            cache_miss_warning: false,
            session: Tracked::new(SessionManager::new()),
            ask_user: AskUserState::new(),
            backend: AgentBackend::Local,
            runtime: AgentRuntime::new(),
            step_back: StepBackState::default(),
            theme: Theme::default(),
            display,
            mouse_select: MouseSelectState::new(),
            sandbox: false,
        }
    }

    /// Returns true when an agent turn is active (streaming or waiting for first token).
    pub fn streaming(&self) -> bool {
        self.agent_turn.is_active()
    }

    /// Advance the throbber animation frame.  Called on every UI tick.
    pub fn tick(&mut self) {
        self.agent_turn.advance_tick();
    }

    /// Record a model/provider change in the event log.
    ///
    /// Call this whenever `current_model` or `current_provider` is updated so
    /// that the change is preserved in the session history.
    pub fn record_model_changed(&mut self) {
        self.append_event_immediate(SessionEvent::ModelChanged {
            model: self.provider.current_model.clone(),
            provider: self.provider.current_instance.id.clone(),
            timestamp: Self::now_ts(),
        });
    }

    /// Record a thinking-level change in the event log.
    ///
    /// Call this whenever `current_thinking` is updated.
    pub fn record_thinking_level_changed(&mut self) {
        self.append_event_immediate(SessionEvent::ThinkingLevelChanged {
            level: self.provider.current_thinking,
            timestamp: Self::now_ts(),
        });
    }

    /// Returns true when the throbber should be visible.
    ///
    /// Three-state model:
    /// - Machine waiting for **user** (`has_pending_ask` / `ask_user_freeform_mode`):
    ///   throbber hidden — the ball is in the user's court.
    /// - Machine producing **output** (visible content added very recently):
    ///   throbber hidden — something is actively appearing on screen.
    /// - Machine working **silently** (streaming, no visible output for a short interval):
    ///   throbber visible — signals that work is in progress.
    /// - Token refresh in progress: throbber visible regardless of turn state,
    ///   so the user sees activity during the ~500ms refresh window.
    pub fn throbber_visible(&self) -> bool {
        self.agent_turn
            .throbber_visible(self.has_pending_ask() || self.ask_user_freeform_mode())
    }

    /// Returns true when provider/system status text should be visible.
    pub fn provider_status_visible(&self) -> bool {
        matches!(
            self.agent_turn.status,
            Some(StreamingStatus::Message(_) | StreamingStatus::CompletedMessage(_))
        )
    }

    pub fn ask_user_freeform_mode(&self) -> bool {
        self.ask_user.freeform_mode
    }

    pub fn queued_steering(&self) -> &[String] {
        self.runtime.queued_steering()
    }

    /// Toggle the info bar visibility.
    pub fn toggle_info(&mut self) {
        self.show_info = !self.show_info;
    }

    // ── Agent switching ───────────────────────────────────────────────────────

    /// Switch to the named agent, rebuilding tools, skills, and system prompt.
    /// Passing an empty string clears the active agent and restores defaults.
    /// Persists the choice to config.toml.
    pub fn switch_agent(&mut self, name: &str, cwd: &str) {
        if name.is_empty() {
            self.active_agent = None;
        } else if self.agents.iter().any(|a| a.name == name) {
            self.active_agent = Some(name.to_string());
        } else {
            return; // unknown agent name — ignore
        }
        self.rebuild_agent_system_prompt(cwd);

        // Persist to config
        if let Ok(mut config) = crate::config::RiConfig::load() {
            config.agent = self.active_agent.clone();
            let _ = config.save();
        }
    }

    /// Rebuild `agent_config.system_prompt` from the currently active agent,
    /// or the default when none is active.  Skills and tools in the prompt
    /// are filtered according to the active agent's include/exclude rules.
    pub(crate) fn rebuild_agent_system_prompt(&mut self, cwd: &str) {
        let agent = self.resolve_current_agent();
        let skills: Vec<crate::skills::SkillMeta> = if let Some(a) = agent {
            crate::agents::filter_skills(&self.loaded_skills, &a.include_skills, &a.exclude_skills)
        } else {
            self.loaded_skills.clone()
        };
        let system_prompt =
            crate::agent::build_system_prompt(&self.agent_config.tools, cwd, &skills, agent);
        self.agent_config.system_prompt = Some(system_prompt);
    }

    /// Return a reference to the currently active agent.
    ///
    /// Falls back to the "default" agent (if present in the agents list) when
    /// no agent has been explicitly selected.
    pub fn resolve_current_agent(&self) -> Option<&crate::agents::AgentMeta> {
        if let Some(name) = self.active_agent.as_deref() {
            self.agents.iter().find(|a| a.name == name)
        } else {
            self.agents.iter().find(|a| a.name == "default")
        }
    }

    /// Return the list of primary agents for the picker.
    pub fn primary_agents(&self) -> Vec<&crate::agents::AgentMeta> {
        self.agents
            .iter()
            .filter(|a| matches!(a.mode, crate::agents::AgentMode::Primary))
            .collect()
    }

    pub(crate) async fn recv_app_event(&mut self) -> Option<AppEvent> {
        self.runtime.recv_app_event().await
    }

    pub fn app_event_tx(&self) -> AppEventTx {
        self.runtime.app_event_tx()
    }

    pub fn init_session_persistence(&mut self, cwd: String) {
        self.session.current_cwd = cwd;
        match SessionStore::open() {
            Ok(store) => {
                self.session.session_store = Some(store);
                self.refresh_resume_availability();
            }
            Err(e) => {
                log::debug!("session persistence disabled: {}", e);
                self.session
                    .live_turn
                    .notices
                    .push(Message::assistant(format!(
                        "[session persistence unavailable: {e}]"
                    )));
            }
        }
    }

    /// Return all messages to display in the chat log: committed session
    /// messages followed by the live turn overlay (streaming assistant,
    /// in-flight tools, and UI-only notices).
    pub fn display_messages_combined(&self) -> Vec<Message> {
        let committed = self
            .session
            .session_state
            .as_ref()
            .map(|s| s.display_messages())
            .unwrap_or(&[]);
        compose_display(committed, &self.session.live_turn, self.streaming())
    }

    /// When in step-back mode, returns `(kept_messages, discarded_messages)`.
    /// `kept_messages` covers events before the step cursor (rendered normally).
    /// `discarded_messages` covers events from the step cursor onward (rendered dimmed).
    /// Returns `None` when not stepping.
    pub fn display_messages_split(&self) -> Option<(Vec<Message>, Vec<Message>)> {
        let idx = self.step_back.cursor?;
        let ss = self.session.session_state.as_ref()?;
        let events = ss.events();
        let kept = crate::projection::project_display_messages(&events[..idx]);
        let discarded = crate::projection::project_display_messages(&events[idx..]);
        Some((kept, discarded))
    }

    /// Push a transient UI-only notice (not backed by a `SessionEvent`).
    pub fn push_notice(&mut self, msg: Message) {
        self.session.live_turn.notices.push(msg);
    }

    /// Whether there are no committed display messages and no live overlay.
    pub fn display_is_empty(&self) -> bool {
        self.session
            .session_state
            .as_ref()
            .map(|s| s.display_is_empty())
            .unwrap_or(true)
            && self.session.live_turn.notices.is_empty()
            && !self.session.live_turn.has_assistant_content()
            && !self.session.live_turn.has_tool_entries()
    }

    /// Number of displayed messages (committed + live overlay).
    pub fn display_len(&self) -> usize {
        let committed = self
            .session
            .session_state
            .as_ref()
            .map(|s| s.display_len())
            .unwrap_or(0);
        // Use streaming=false for counting purposes (we don't want the
        // waiting-cursor empty slot to affect the count used for shell IDs).
        committed + self.session.live_turn.render_overlay(false).len()
    }

    pub fn should_show_resume_hint(&self) -> bool {
        self.session.resume_available_for_cwd
            && self.display_is_empty()
            && self.ui_is_suspend_idle()
            && !self.streaming()
    }

    pub(crate) fn ui_is_suspend_idle(&self) -> bool {
        !self.selection.active
            && self.provider.setup_step == ProviderSetupStep::Idle
            && self.input_mode == InputMode::Chat
    }

    pub fn resume_latest_for_current_cwd(&mut self) {
        let Some(store) = self.session.session_store.as_ref() else {
            return;
        };
        let Some(meta) = store.latest_for_cwd(&self.session.current_cwd) else {
            self.session.live_turn.notices.push(Message::assistant(
                "[no resumable session in this working folder]",
            ));
            return;
        };
        self.resume_session_by_id(&meta.id);
    }

    pub fn resume_session_by_id(&mut self, session_id: &str) {
        let Some(store) = self.session.session_store.as_ref() else {
            return;
        };
        match store.load_events(session_id) {
            Ok(log) => {
                // Capture last known token usage from the loaded events
                // before moving the event log into session_state.
                self.latest_usage = Self::find_last_usage_from_events(&log.events);
                self.session.session_state = Some(SessionState::from_event_log(log));
                self.session.live_turn.clear_all();
                self.session.current_session_id = Some(session_id.to_string());
                self.log_view.auto_scroll = true;
                self.log_view.log_scroll = 0;
            }
            Err(e) => {
                self.session
                    .live_turn
                    .notices
                    .push(Message::assistant(format!(
                        "[failed to resume session: {e}]"
                    )));
            }
        }
        self.refresh_resume_availability();
    }

    /// Scan session events for the last known token usage data.
    ///
    /// Checks the most recent [`AssistantMessage`] that has `usage` data,
    /// then falls back to the most recent [`CompactionSummary`]'s
    /// `tokens_after` so that the info bar can show a meaningful context
    /// utilisation value immediately on session resume.
    ///
    /// [`AssistantMessage`]: SessionEvent::AssistantMessage
    /// [`CompactionSummary`]: SessionEvent::CompactionSummary
    fn find_last_usage_from_events(events: &[SessionEvent]) -> Option<UsageStats> {
        for ev in events.iter().rev() {
            match ev {
                SessionEvent::AssistantMessage { usage: Some(u), .. } => return Some(*u),
                SessionEvent::CompactionSummary { tokens_after, .. } => {
                    return Some(UsageStats {
                        input_tokens: Some(*tokens_after),
                        output_tokens: None,
                        total_tokens: Some(*tokens_after),
                        cached_tokens: None,
                        reasoning_tokens: None,
                    });
                }
                _ => {}
            }
        }
        None
    }

    pub fn enter_resume_selection_mode(&mut self) {
        self.reset_textarea();
        self.session.live_turn.notices.clear();

        let items = if let Some(store) = self.session.session_store.as_ref() {
            let current_folder = std::path::Path::new(&self.session.current_cwd)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            let mut sessions = store.list_sessions();
            sessions.sort_by(|a, b| {
                let a_scope = session_scope(&a.cwd, &self.session.current_cwd, current_folder);
                let b_scope = session_scope(&b.cwd, &self.session.current_cwd, current_folder);
                a_scope
                    .cmp(&b_scope)
                    .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
            });

            if sessions.is_empty() {
                vec![CompletionItem {
                    label: "no saved sessions yet".to_string(),
                    detail: String::new(),
                    complete_to: String::new(),
                    loading: true,
                    error: false,
                    match_range: None,
                }]
            } else {
                sessions
                    .iter()
                    .map(|meta| {
                        let scope = session_scope_label(
                            &meta.cwd,
                            &self.session.current_cwd,
                            current_folder,
                        );
                        let when = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                            meta.updated_at_ms,
                        )
                        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "unknown time".to_string());
                        let prompt_hint = meta.first_prompt.as_deref().unwrap_or(&meta.id);

                        CompletionItem {
                            label: format!("[{scope}] {when}  —  {prompt_hint}"),
                            detail: format!("{} msgs • {}", meta.message_count, meta.cwd),
                            complete_to: format!("/resume_session {}", meta.id),
                            loading: false,
                            error: false,
                            match_range: None,
                        }
                    })
                    .collect()
            }
        } else {
            vec![CompletionItem {
                label: "session persistence unavailable".to_string(),
                detail: String::new(),
                complete_to: String::new(),
                loading: true,
                error: false,
                match_range: None,
            }]
        };
        self.selection
            .activate(SelectionKind::ResumeSession, "  Resume session  ", items);
    }

    pub fn enter_keybinding_help_mode(&mut self) {
        self.session.live_turn.notices.clear();

        let contexts = [
            BindingContext::Global,
            BindingContext::Chat,
            BindingContext::Selection,
            BindingContext::Shell,
            BindingContext::ProviderPicker,
            BindingContext::Mouse,
        ];

        let mut items = Vec::new();
        for context in contexts {
            let bindings: Vec<_> = KEYBINDINGS
                .iter()
                .filter(|binding| binding.context == context)
                .collect();
            if bindings.is_empty() {
                continue;
            }

            items.push(CompletionItem {
                label: context.label().to_string(),
                detail: String::new(),
                complete_to: String::new(),
                loading: true,
                error: false,
                match_range: None,
            });

            items.extend(bindings.into_iter().map(|binding| CompletionItem {
                label: binding.shortcut.to_string(),
                detail: binding.description.to_string(),
                complete_to: String::new(),
                loading: false,
                error: false,
                match_range: None,
            }));
        }

        self.selection.activate(
            SelectionKind::KeybindingHelp,
            "  Keyboard shortcuts  ",
            items,
        );
        self.selection_select_next();
    }

    pub(crate) fn make_textarea() -> TextArea<'static> {
        TextArea::default()
    }

    /// Reset the chat input area to a blank state between submissions.
    /// Also clears any active completion state.
    pub fn reset_textarea(&mut self) {
        self.textarea = Self::make_textarea();
        self.completion.clear();
    }

    pub fn shell_input_is_empty(&self) -> bool {
        self.shell.input_is_empty()
    }

    pub fn enter_shell_mode(&mut self) {
        self.input_mode = InputMode::Shell;
        self.shell.reset_textarea();
        self.completion.clear();
    }

    pub fn exit_shell_mode(&mut self) {
        self.input_mode = InputMode::Chat;
        self.shell.reset_textarea();
    }

    pub fn submit_shell_command(&mut self) {
        let lines: Vec<String> = self.shell.textarea.lines().to_vec();
        let command = lines.join("\n").trim().to_string();
        if command.is_empty() || self.streaming() {
            return;
        }

        // Ensure a session exists so the appended events are persisted.
        self.ensure_event_log_for_submit();

        let cwd = if self.session.current_cwd.is_empty() {
            ".".to_string()
        } else {
            self.session.current_cwd.clone()
        };
        let prompt = '$';
        let cmd_prefix = format!("{cwd}{prompt}");

        let call_id = format!("local-shell-{}", self.display_len());

        // Push a live tool entry so the UI renders the tool-call header and
        // live streaming output (via ToolOutputChunk events forwarded from the
        // subprocess).
        self.session
            .live_turn
            .tool_entries
            .push(crate::live_turn::LiveToolEntry {
                id: call_id.clone(),
                name: "local_shell".to_string(),
                args: serde_json::json!({
                    "prefix": cmd_prefix,
                    "command": command,
                }),
                partial_args: String::new(),
                partial_snapshot: None,
                streaming_field: Some("command".to_string()),
                running_output: String::new(),
                last_output_line_count: 0,
                result: None,
            });

        self.exit_shell_mode();
        self.log_view.auto_scroll = true;

        let tx = self.app_event_tx();
        let ctx = crate::agent::types::ToolCallContext {
            id: call_id.clone(),
            tx: Some(tx.clone()),
            cancel_rx: None,
            subagent: None,
            root: None,
            // Interactive slash-mode shell commands stay on the host: they are
            // user-initiated convenience commands (git, dnf, …) that would be
            // surprising inside the sandbox.
            sandbox: false,
        };

        self.runtime.pending_shell_handle = Some(tokio::spawn(async move {
            let cmd = crate::agent::tools::subprocess::SubprocessCommand::new("sh")
                .arg("-c")
                .arg(&command);

            let result = cmd.current_dir(&cwd).run(ctx).await;
            let _ = tx.send(AppEvent::ShellComplete { call_id, result });
        }));
    }

    /// True when the input is a single line beginning with `/`.
    pub fn in_slash_mode(&self) -> bool {
        let lines = self.textarea.lines();
        lines.len() == 1 && lines[0].trim_start().starts_with('/')
    }

    /// Resolve which slash-command text should execute when Enter is pressed.
    ///
    /// If a completion row is highlighted, prefer its `complete_to` text so
    /// partial inputs like `/mo` execute the selected command immediately.
    pub fn slash_submit_text(&self) -> Option<String> {
        let lines = self.textarea.lines();
        if lines.len() != 1 {
            return None;
        }

        let raw = lines[0].trim().to_string();
        if !raw.starts_with('/') {
            return None;
        }

        if let Some(item) = self
            .completion
            .completions
            .get(self.completion.completion_selected)
            && !item.loading
            && !item.complete_to.is_empty()
        {
            return Some(item.complete_to.trim_end().to_string());
        }

        Some(raw)
    }

    /// Handle `Esc` in normal chat-input mode (outside shell/selection).
    ///
    /// Priority order is intentional:
    /// 1) cancel pending ask
    /// 2) cancel slash-command input/completion
    /// 3) cancel provider-name input
    /// 4) cancel provider endpoint setup input
    /// 5) when streaming, show a Ctrl-C hint instead of aborting
    /// 6) when idle, clear non-empty input
    pub fn handle_escape_in_chat_mode(&mut self) {
        if self.is_stepping() {
            // Clean up any in-flight ask_user UI state before cancelling.
            if self.has_pending_ask() {
                self.ask_user.pending = None;
                self.ask_user.freeform_mode = false;
                self.exit_selection_mode();
            }
            self.cancel_stepping();
        } else if self.has_pending_ask() {
            self.cancel_pending_ask();
        } else if self.in_slash_mode() {
            self.reset_textarea();
        } else if self.selection.kind == Some(SelectionKind::ConfirmProviderRemoval) {
            self.exit_selection_mode();
            self.clear_pending_provider_removal();
        } else if self.provider.setup_step != ProviderSetupStep::Idle {
            self.cancel_setup_input();
        } else if self.streaming() {
            // Esc no longer aborts the agent loop — use Ctrl-C for that.
            self.push_notice(Message::assistant(
                "[Use Ctrl-C to abort the agent loop]".to_string(),
            ));
        } else {
            // Idle: clear input if non-empty, no-op if empty (exception to
            // the feedback invariant — base state with nothing to dismiss).
            let input_is_empty = self
                .textarea
                .lines()
                .iter()
                .all(|line| line.trim().is_empty());
            if !input_is_empty {
                self.reset_textarea();
            }
        }
    }

    // ── Completion helpers ────────────────────────────────────────────────────

    /// Recompute the completion list from the current textarea content and
    /// cached model list. Call this after every keystroke.
    pub fn update_completions(&mut self) {
        let cwd = self.session.current_cwd.clone();
        self.completion.update(
            &self.textarea,
            &self.loaded_skills,
            self.provider.thinking_supported,
            &self.provider.instances,
            &cwd,
        );
    }

    /// Returns true if a model-list fetch should be triggered now.
    /// Returns true when a model-list fetch should be triggered automatically.
    ///
    /// Fires when no fetch is already in-flight and the model list has not yet
    /// been populated.  This covers two cases:
    /// 1. No model configured — the list is needed so the user can pick one.
    /// 2. Model configured — the fetch is still beneficial because it populates
    ///    the provider model metadata cache (context-window size, vendor) from
    ///    the live API, which otherwise falls back to the hard-coded table.
    ///
    /// Does not trigger when no provider has been selected — on a clean
    /// install the login menu is shown instead.
    pub fn should_auto_query_model(&self) -> bool {
        self.provider.provider_selected
            && !self.completion.models_loading
            && self.completion.available_models.is_none()
            && self.selection.kind != Some(SelectionKind::Model)
    }

    pub fn should_fetch_models(&self) -> bool {
        if self.completion.available_models.is_some()
            || self.completion.models_loading
            || self.completion.model_fetch_error.is_some()
        {
            return false;
        }
        let lines = self.textarea.lines();
        lines.len() == 1 && lines[0].trim_start().starts_with("/model ")
    }

    /// Spawn a background task to fetch the model list from the provider.
    pub fn start_model_fetch(&mut self, provider: &DynProvider) {
        self.completion.models_loading = true;
        self.completion.model_fetch_error = None;
        let future = provider.list_models();
        let tx = self.app_event_tx();
        tokio::spawn(async move {
            let result = future.await;
            let _ = tx.send(AppEvent::ModelsReady(result));
        });
    }

    /// Store a freshly fetched model list (or error) and refresh completions.
    pub fn apply_model_list(&mut self, result: Result<Vec<String>, crate::llm::ProviderError>) {
        self.completion.models_loading = false;
        match result {
            Ok(models) => {
                self.completion.available_models = Some(models);
                self.completion.model_fetch_error = None;
            }
            Err(e) => {
                let provider_label = active_provider_display_name(
                    &self.provider.current_instance.id,
                    &self.provider.instances,
                );
                self.completion.model_fetch_error =
                    Some(format_provider_error_for_display(&provider_label, &e));
            }
        }
        self.update_completions();

        // If no model is configured and the fetch succeeded, open the model
        // picker automatically so the user can choose one.
        if self.provider.current_instance.model.is_none()
            && self.completion.available_models.is_some()
            && !self.selection.active
        {
            self.enter_model_selection_mode();
            return;
        }

        if self.selection.active && self.selection.kind == Some(SelectionKind::Model) {
            if let Some(err) = &self.completion.model_fetch_error {
                let items = vec![completion::CompletionItem::error_indicator(err)];
                self.set_selection_items(items);
            } else {
                let items: Vec<_> = self
                    .completion
                    .available_models
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|m| completion::CompletionItem::from_model(m))
                    .collect();
                if !items.is_empty() {
                    self.set_selection_items(items);
                    if self.selection.query.trim().is_empty() {
                        self.select_current_default();
                    }
                }
            }
        }
    }

    /// Navigate the completion selection down (wraps around).
    pub fn completion_select_next(&mut self) {
        self.completion.select_next();
    }

    /// Navigate the completion selection up (wraps around).
    pub fn completion_select_prev(&mut self) {
        self.completion.select_prev();
    }

    /// Replace the textarea with the selected item's `complete_to` text and
    /// move the cursor to the end of the line. No-ops on loading indicators.
    ///
    /// For `@<file>` completions, replaces only the `@` token portion of the
    /// input rather than the entire textarea, preserving surrounding text.
    pub fn apply_completion(&mut self) {
        let item = match self
            .completion
            .completions
            .get(self.completion.completion_selected)
        {
            Some(i) if !i.loading && !i.complete_to.is_empty() => i,
            _ => return,
        };

        let lines: Vec<String> = self.textarea.lines().to_vec();
        let input = lines.join("\n");

        // Check if the textarea contains an @ token that triggered file completions.
        if let Some(range) = Self::find_at_token(&input) {
            // Replace just the @token portion with @ + completed path.
            let completed_path = &item.complete_to;
            let new_text = format!(
                "{}@{}{}",
                &input[..range.0],
                completed_path,
                &input[range.1..]
            );
            self.textarea = TextArea::new(new_text.lines().map(|s| s.to_string()).collect());
        } else {
            // Standard completion: replace entire textarea.
            let text = item.complete_to.clone();
            self.textarea = TextArea::new(vec![text]);
        }

        self.textarea.move_cursor(CursorMove::End);
        self.update_completions();
    }

    /// Find the byte range of the last `@<path>` token in `input`.
    ///
    /// Returns `(start, end)` where `start` is the position of `@` and `end`
    /// is the position after the end of the path fragment.  The token must be
    /// preceded by start-of-string or ASCII whitespace.
    fn find_at_token(input: &str) -> Option<(usize, usize)> {
        let bytes = input.as_bytes();
        let len = bytes.len();
        let mut i = 0;
        let mut result: Option<(usize, usize)> = None;

        while i < len {
            if bytes[i] == b'@' {
                let preceded_by_space = i == 0 || bytes[i - 1].is_ascii_whitespace();
                if preceded_by_space && i + 1 < len {
                    let start = i;
                    let mut end = i + 1;
                    while end < len && !bytes[end].is_ascii_whitespace() && bytes[end] != b'"' {
                        end += 1;
                    }
                    result = Some((start, end));
                    i = end;
                    continue;
                }
            }
            i += 1;
        }

        result
    }

    // ── Step-back navigation ──────────────────────────────────────────────────

    /// Returns the event indices (into the committed event log) of all
    /// step-back boundaries: `UserMessage` events and `ToolResult` events
    /// for `ask_user` (user answers to in-turn questions), in order.
    pub(crate) fn step_boundaries(&self) -> Vec<usize> {
        let Some(ss) = self.session.session_state.as_ref() else {
            return Vec::new();
        };
        ss.events()
            .iter()
            .enumerate()
            .filter_map(|(i, ev)| match ev {
                SessionEvent::UserMessage { .. } => Some(i),
                SessionEvent::ToolResult { name, .. } if name == "ask_user" => Some(i),
                _ => None,
            })
            .collect()
    }

    /// Returns true when step-back navigation is currently active.
    pub(crate) fn is_stepping(&self) -> bool {
        self.step_back.is_stepping()
    }

    /// Step back to the previous step boundary (user message or ask_user answer).
    ///
    /// On the first call, saves the current input field content so it can be
    /// restored on cancel.  No-op when the agent loop is active or there are no
    /// boundaries.
    pub(crate) fn step_back(&mut self) {
        if self.runtime.is_running() {
            return;
        }
        let boundaries = self.step_boundaries();
        if boundaries.is_empty() {
            return;
        }
        // Clear any in-flight ask_user UI from a previous step before
        // repopulating with the new boundary.
        if self.has_pending_ask() && self.ask_user.reply.is_none() {
            self.ask_user.pending = None;
            self.ask_user.freeform_mode = false;
            self.exit_selection_mode();
        }
        let current = self.step_back.cursor;
        let new_cursor = match current {
            None => {
                // Save current input before first step
                self.step_back.save_input(self.textarea.lines().join("\n"));
                // Step to the last UserMessage
                *boundaries.last().unwrap()
            }
            Some(cur) => {
                // Find the boundary strictly before cur
                match boundaries.iter().rev().find(|&&i| i < cur) {
                    Some(&i) => i,
                    None => return, // Already at the earliest boundary
                }
            }
        };
        self.step_back.cursor = Some(new_cursor);
        self.repopulate_input_from_cursor();
        self.scroll_to_step_cursor();
        self.log_view.invalidate();
    }

    /// Step forward toward the next step boundary.
    ///
    /// When stepping past the end, clears step state and restores the saved
    /// input.  No-op when the agent loop is active.
    pub(crate) fn step_forward(&mut self) {
        if self.runtime.is_running() {
            return;
        }
        let Some(cur) = self.step_back.cursor else {
            return;
        };
        // Clear any in-flight ask_user UI from a previous step before
        // repopulating with the new boundary.
        if self.has_pending_ask() && self.ask_user.reply.is_none() {
            self.ask_user.pending = None;
            self.ask_user.freeform_mode = false;
            self.exit_selection_mode();
        }
        let boundaries = self.step_boundaries();
        match boundaries.iter().find(|&&i| i > cur) {
            Some(&next) => {
                self.step_back.cursor = Some(next);
                self.repopulate_input_from_cursor();
                self.scroll_to_step_cursor();
                self.log_view.invalidate();
            }
            None => {
                // Past the end — cancel stepping
                self.cancel_stepping();
            }
        }
    }

    /// Cancel step-back mode, restoring the original input and full view.
    pub(crate) fn cancel_stepping(&mut self) {
        if let Some(saved) = self.step_back.cancel() {
            self.textarea = TextArea::new(vec![saved]);
            self.textarea.move_cursor(CursorMove::End);
        }
        self.log_view.auto_scroll = true;
        self.log_view.invalidate();
    }

    /// Repopulate the input field with the user-provided content at the
    /// current step cursor position (user message text or ask_user answer).
    ///
    /// For `ask_user` tool results, instead of populating the textarea,
    /// restores the full ask_user prompt UI so the user can answer fresh.
    fn repopulate_input_from_cursor(&mut self) {
        let Some(idx) = self.step_back.cursor else {
            return;
        };
        let Some(ss) = self.session.session_state.as_ref() else {
            return;
        };
        match ss.events().get(idx) {
            Some(SessionEvent::UserMessage { content, .. }) => {
                let text = content.clone();
                self.textarea = TextArea::new(vec![text]);
                self.textarea.move_cursor(CursorMove::End);
            }
            Some(SessionEvent::ToolResult { id, name, .. }) if name == "ask_user" => {
                // Find the preceding ToolCall with matching id to get the
                // question and options.
                let tool_call_args = ss.events()[..idx].iter().rev().find_map(|ev| match ev {
                    SessionEvent::ToolCall {
                        id: tid,
                        name: tname,
                        args,
                        ..
                    } if tid == id && tname == "ask_user" => Some(args.clone()),
                    _ => None,
                });
                if let Some(args) = tool_call_args {
                    self.restore_ask_user_from_step(&args);
                }
            }
            _ => {}
        }
    }

    /// Adjust the scroll position so the step cursor boundary is visible with
    /// context on both sides.
    fn scroll_to_step_cursor(&mut self) {
        self.log_view.auto_scroll = false;
        // The exact line count for the kept portion is not known until render
        // time; we set auto_scroll to false and let the render path handle
        // final clamping.  For now, a coarse approximation: scroll to the end
        // of the kept portion.  The render path will center it properly.
        self.log_view.log_scroll = usize::MAX; // will be clamped in draw
    }

    /// Commit the step: create a new branched session from events up to (but
    /// not including) the step cursor, plus the current textarea content as a
    /// new `UserMessage`.  Switches the active session to the branch.
    ///
    /// Returns the new `UserMessage` content, or `None` if not in step mode or
    /// session state is unavailable.
    pub(crate) fn commit_step_branch(&mut self) -> Option<String> {
        let idx = self.step_back.cursor?;
        let new_content = self.textarea.lines().join("\n");
        if new_content.trim().is_empty() {
            return None;
        }

        let events: Vec<SessionEvent> = {
            let ss = self.session.session_state.as_ref()?;
            ss.events()[..idx].to_vec()
        };

        let cwd = self.session.current_cwd.clone();
        let new_session_id = self
            .session
            .session_store
            .as_mut()
            .and_then(|store| store.create_session_from_events(&cwd, &events).ok());

        // Switch active session
        if let Some(ref session_id) = new_session_id
            && let Some(store) = &self.session.session_store
            && let Ok(log) = store.load_events(session_id)
        {
            self.session.current_session_id = Some(session_id.clone());
            self.session.session_state =
                Some(crate::session_state::SessionState::from_event_log(log));
            self.session.live_turn = crate::live_turn::LiveTurnState::new();
            self.session.pending_turn_events.clear();
        }

        // Clear step state
        self.step_back.clear();
        self.log_view.auto_scroll = true;
        self.log_view.invalidate();

        Some(new_content)
    }

    /// Copy the last assistant response to the system clipboard.
    ///
    /// Prefers the currently streaming (live turn) assistant content; falls
    /// back to the last committed assistant message.  Silently does nothing
    /// if there is no assistant content to copy.
    pub fn copy_last_assistant_response(&mut self) {
        let text = if self.session.live_turn.has_assistant_content() {
            self.session.live_turn.assistant_content.clone()
        } else if let Some(ss) = self.session.session_state.as_ref() {
            let msgs = ss.display_messages();
            msgs.iter()
                .rev()
                .find(|m| m.role == crate::llm::Role::Assistant)
                .map(|m| m.content.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }

        match crate::clipboard::set_clipboard(&text) {
            Ok(()) => {
                log::debug!("copied {} bytes to clipboard via OSC 52", text.len());
                self.agent_turn
                    .set_status(Some(StreamingStatus::CompletedMessage(
                        "📋 Copied to clipboard.".to_string(),
                    )));
            }
            Err(e) => {
                log::debug!("clipboard copy failed: {e}");
            }
        }
    }
}

/// Return a sort key for session scope: 0 = local, 1 = similar, 2 = foreign.
fn session_scope(session_cwd: &str, current_cwd: &str, current_folder: &str) -> u8 {
    if session_cwd == current_cwd {
        0 // local
    } else if !current_folder.is_empty() && folder_name(session_cwd) == Some(current_folder) {
        1 // similar
    } else {
        2 // foreign
    }
}

/// Return a human-readable scope label for a session.
fn session_scope_label(session_cwd: &str, current_cwd: &str, current_folder: &str) -> &'static str {
    if session_cwd == current_cwd {
        "local"
    } else if !current_folder.is_empty() && folder_name(session_cwd) == Some(current_folder) {
        "similar"
    } else {
        "foreign"
    }
}

/// Extract the final path component as `Some(&str)`, or `None` for empty/"." paths.
fn folder_name(path: &str) -> Option<&str> {
    let p = std::path::Path::new(path);
    let name = p.file_name()?.to_str()?;
    if name.is_empty() || name == "." || name == ".." {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::App;
    use crate::{
        agent::AgentLoopConfig,
        provider_instance::{ApiType, BackendPreset, ProviderInstance},
        provider_manager::{PendingProviderSetup, SetupInputKind},
        thinking::ThinkingLevel,
    };

    fn make_app() -> App {
        let instance = crate::provider_instance::ProviderInstance::new(
            "openai",
            BackendPreset::OpenAiCompatible,
        );
        App::new(
            instance,
            "gpt-4o",
            ThinkingLevel::Off,
            AgentLoopConfig {
                tools: Default::default(),
                file_tracker: std::sync::Arc::new(std::sync::Mutex::new(
                    crate::agent::FileTracker::new(),
                )),
                tool_output_log: std::sync::Arc::new(std::sync::Mutex::new(
                    crate::agent::ToolOutputLog::new("test-session"),
                )),
                session_events: vec![],
                current_model: "gpt-4o".to_string(),
                auto_compaction_enabled: true,
                manual_compaction_instructions: None,
                executor: std::sync::Arc::new(crate::agent::DefaultToolExecutor::new()),
                system_prompt: None,
            },
            crate::config::DisplayConfig::default(),
        )
    }

    #[test]
    fn fresh_app_has_no_provider_selected() {
        let app = make_app();
        assert!(
            !app.provider.provider_selected,
            "fresh App should not have a provider selected"
        );
    }

    #[test]
    fn ui_is_suspend_idle_only_in_plain_chat_idle_state() {
        let mut app = make_app();
        assert!(app.ui_is_suspend_idle());

        app.input_mode = super::InputMode::Shell;
        assert!(!app.ui_is_suspend_idle());
        app.input_mode = super::InputMode::Chat;

        app.selection.active = true;
        assert!(!app.ui_is_suspend_idle());
        app.selection.active = false;

        app.provider.setup_step = crate::provider_manager::ProviderSetupStep::Endpoint;
        assert!(!app.ui_is_suspend_idle());
        app.provider.setup_step = crate::provider_manager::ProviderSetupStep::Idle;

        assert!(app.ui_is_suspend_idle());
    }

    #[test]
    fn should_show_resume_hint_stays_hidden_while_streaming() {
        let mut app = make_app();
        app.session.resume_available_for_cwd = true;
        assert!(app.display_is_empty());
        assert!(app.ui_is_suspend_idle());
        assert!(!app.streaming());
        assert!(app.should_show_resume_hint());

        app.agent_turn.start();
        assert!(app.streaming());
        assert!(!app.should_show_resume_hint());
    }

    #[test]
    fn submit_chat_message_blocked_when_no_provider_selected() {
        let mut app = make_app();
        assert!(!app.provider.provider_selected);

        let provider = std::sync::Arc::new(crate::llm::test_provider::TestProvider::new())
            as std::sync::Arc<dyn crate::llm::LlmProvider + Send + Sync>;
        // Populate the textarea with a non-empty message.
        app.textarea.insert_str("hello");

        // Should push a notice rather than submit.
        let before = app.session.live_turn.notices.len();
        app.submit_chat_message(&provider);
        assert_eq!(
            app.session.live_turn.notices.len(),
            before + 1,
            "should have pushed a notice"
        );
        assert!(
            app.session
                .live_turn
                .notices
                .last()
                .unwrap()
                .content
                .contains("no provider selected"),
            "notice should mention no provider selected"
        );
        // Pending finalise should NOT be set (no real submission happened).
        assert!(!app.runtime.pending_finalize);

        // Now set provider_selected and verify submission proceeds.
        app.provider.provider_selected = true;
        app.submit_chat_message(&provider);
        // The textarea was cleared by the first call; refill.
        app.textarea.insert_str("hello again");
        app.submit_chat_message(&provider);
        assert!(
            app.runtime.pending_finalize,
            "should have triggered submission"
        );
    }

    #[test]
    fn setup_input_kind_uses_generic_openai_compatible_prompts() {
        assert_eq!(
            SetupInputKind::Name.prompt_label(None),
            "provider instance name: "
        );

        let mut compat = ProviderInstance::new("compat", BackendPreset::OpenAiCompatible);
        compat.api_type = ApiType::OpenAiCompatible;
        assert_eq!(SetupInputKind::BaseUrl.prompt_label(Some(&compat)), "URL: ");
        assert_eq!(
            SetupInputKind::ApiKey.prompt_label(Some(&compat)),
            "API key: "
        );

        // Without an instance, fall back to the generic labels.
        assert_eq!(SetupInputKind::BaseUrl.prompt_label(None), "URL: ");
        assert_eq!(SetupInputKind::ApiKey.prompt_label(None), "API key: ");
    }

    #[test]
    fn enter_provider_selection_mode_lists_providers_and_login_entry() {
        let mut app = make_app();
        let providers = vec![
            ProviderInstance::new("openai", BackendPreset::OpenAiCompatible),
            ProviderInstance::new("gpu-box", BackendPreset::OpenAiCompatible),
            ProviderInstance::new("work-webui", BackendPreset::OpenAiCompatible),
        ];

        app.enter_provider_selection_mode(&providers);

        let items: Vec<_> = app
            .selection
            .items
            .iter()
            .map(|item| item.complete_to.as_str())
            .collect();

        assert!(items.contains(&"/provider openai"));
        assert!(items.contains(&"/provider gpu-box"));
        assert!(items.contains(&"/provider work-webui"));
    }

    #[test]
    fn enter_provider_removal_confirmation_mode_tracks_target_provider() {
        let mut app = make_app();
        let instance = ProviderInstance::new("gpu-box", BackendPreset::OpenAiCompatible);

        app.enter_provider_removal_confirmation_mode(&instance);

        assert_eq!(
            app.selection.kind,
            Some(super::SelectionKind::ConfirmProviderRemoval)
        );
        assert_eq!(app.selection.title, "  Remove provider?  ");
        assert_eq!(
            app.provider
                .pending_removal
                .as_ref()
                .map(|pending| pending.id.as_str()),
            Some("gpu-box")
        );
        let items: Vec<_> = app
            .selection
            .items
            .iter()
            .map(|item| item.complete_to.as_str())
            .collect();
        assert_eq!(
            items,
            vec!["/provider_remove_confirm", "/provider_remove_cancel"]
        );
    }

    #[test]
    fn apply_selection_returns_remove_provider_confirmation() {
        let mut app = make_app();
        let instance = ProviderInstance::new("gpu-box", BackendPreset::OpenAiCompatible);
        app.enter_provider_removal_confirmation_mode(&instance);
        app.selection.selected = 0;

        let result = app.apply_selection();

        assert!(matches!(
            result,
            Some(super::SelectionResult::RemoveProvider(id)) if id == "gpu-box"
        ));
    }

    #[test]
    fn apply_selection_returns_cancel_provider_removal() {
        let mut app = make_app();
        let instance = ProviderInstance::new("gpu-box", BackendPreset::OpenAiCompatible);
        app.enter_provider_removal_confirmation_mode(&instance);
        app.selection.selected = 1;

        let result = app.apply_selection();

        assert!(matches!(
            result,
            Some(super::SelectionResult::CancelProviderRemoval)
        ));
    }

    #[test]
    fn clear_pending_provider_setup_clears_pending_provider_removal() {
        let mut app = make_app();
        let instance = ProviderInstance::new("gpu-box", BackendPreset::OpenAiCompatible);
        app.enter_provider_removal_confirmation_mode(&instance);

        app.clear_pending_provider_setup();

        assert!(app.provider.pending_removal.is_none());
    }

    #[test]
    fn submit_pending_provider_base_url_stores_openai_compatible_endpoint() {
        let mut app = make_app();
        app.provider.pending_setup = Some(PendingProviderSetup::new("test".to_string()));
        app.set_pending_provider_backend_preset(BackendPreset::OpenAiCompatible);
        app.enter_provider_endpoint_input_mode();
        app.textarea.insert_str("test");

        let url = app
            .submit_pending_provider_base_url()
            .expect("normalized endpoint url");
        // A pathless endpoint gets the `/v1` prefix automatically.
        assert_eq!(url, "https://test/v1");
        assert_eq!(
            app.pending_provider_instance()
                .as_ref()
                .and_then(|p| p.base_url.as_deref()),
            Some("https://test/v1")
        );
    }

    #[test]
    fn submit_pending_provider_base_url_stores_openrouter_endpoint() {
        let mut app = make_app();
        app.provider.pending_setup = Some(PendingProviderSetup::new("router".to_string()));
        app.set_pending_provider_backend_preset(BackendPreset::OpenAiCompatible);
        app.enter_provider_endpoint_input_mode();
        app.textarea.insert_str("openrouter.ai/api/v1");

        let url = app
            .submit_pending_provider_base_url()
            .expect("normalized openrouter url");
        assert_eq!(url, "https://openrouter.ai/api/v1");
        assert_eq!(
            app.pending_provider_instance()
                .as_ref()
                .and_then(|p| p.base_url.as_deref()),
            Some("https://openrouter.ai/api/v1")
        );
    }

    #[test]
    fn submit_pending_provider_api_key_stores_token() {
        let mut app = make_app();
        app.provider.pending_setup = Some(PendingProviderSetup::new("test".to_string()));
        app.enter_provider_api_key_input_mode();
        app.textarea.insert_str("sk-test");

        let token = app
            .submit_pending_provider_api_key()
            .expect("provider token");
        assert_eq!(token, "sk-test");
        assert_eq!(
            app.provider
                .pending_setup
                .as_ref()
                .and_then(|p| p.api_key.as_deref()),
            Some("sk-test")
        );
    }

    #[test]
    fn provider_selection_mode_reports_selected_provider_id() {
        let mut app = make_app();
        app.enter_provider_selection_mode(&[
            ProviderInstance::new("openai", BackendPreset::OpenAiCompatible),
            ProviderInstance::new("gpu-box", BackendPreset::OpenAiCompatible),
        ]);
        app.selection.selected = app
            .selection
            .items
            .iter()
            .position(|item| item.complete_to == "/provider gpu-box")
            .expect("provider item present");

        assert!(app.in_provider_selection_mode());
        assert_eq!(app.selected_provider_id(), Some("gpu-box"));
    }
}
