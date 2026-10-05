use std::time::Duration;

use bytes::Bytes;

use crate::api::schema::{
    AgentPromptParams, AgentRenameParams, AgentSendKeysParams, AgentStartParams, AgentTarget,
    PaneReadResult, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

// Codex's Windows input reader does not surface bracketed paste. It detects the prompt as a
// "paste burst" and, while that burst is buffered, rewrites a following Enter into a newline
// instead of submitting. The burst only flushes after an idle timeout, so any size-based delay is
// a timing guess that fails when ConPTY delivery lags it. Codex flushes a buffered burst
// synchronously when it receives a non-character key, so appending one after the paste gives the
// submission a deterministic paste boundary regardless of prompt size or delivery speed.
#[cfg(windows)]
fn append_codex_paste_boundary(runtime: &crate::terminal::TerminalRuntime, text: &mut Vec<u8>) {
    let keys = match crate::app::api_helpers::encode_api_keys(runtime, &["right".to_string()]) {
        Ok(keys) => keys,
        Err(key) => {
            tracing::warn!(key = %key, "failed to encode Codex paste boundary key");
            return;
        }
    };
    if let Some(key) = keys.into_iter().find(|bytes| !bytes.is_empty()) {
        text.extend_from_slice(&key);
    }
}

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        self.reconcile_managed_agent_target(&target.target);
        let agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_acknowledge(
        &mut self,
        id: String,
        params: crate::api::schema::AgentAcknowledgeParams,
    ) -> String {
        let acknowledged = match self.acknowledge_agent_completion(&params.target, &params) {
            Ok(acknowledged) => acknowledged,
            Err(err) => return encode_error_body(id, self.agent_acknowledge_error_body(err)),
        };

        if let Some(update) = acknowledged.state_update.as_ref() {
            self.emit_pane_state_update(update);
        }

        encode_success(
            id,
            ResponseResult::AgentCompletionAcknowledged {
                agent: acknowledged.agent,
                outcome: acknowledged.outcome,
            },
        )
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    pub(crate) fn handle_deferred_agent_api_request(
        &mut self,
        request: crate::api::schema::Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        let crate::api::schema::Method::AgentPrompt(params) = request.method else {
            return false;
        };
        match self.queue_agent_prompt(request.id, params) {
            Ok((id, agent, completion)) => {
                std::thread::spawn(move || {
                    let response = match completion.recv() {
                        Ok(Ok(())) => encode_success(id, ResponseResult::AgentPrompted { agent }),
                        Ok(Err(err)) if err.kind() == std::io::ErrorKind::TimedOut => {
                            encode_error(id, "timeout", err.to_string())
                        }
                        Ok(Err(err)) => encode_error(id, "agent_prompt_failed", err.to_string()),
                        Err(_) => encode_error(id, "agent_prompt_failed", "pty actor closed"),
                    };
                    let _ = respond_to.send(response);
                });
            }
            Err(response) => {
                let _ = respond_to.send(response);
            }
        }
        true
    }

    fn queue_agent_prompt(
        &mut self,
        id: String,
        params: AgentPromptParams,
    ) -> Result<
        (
            String,
            crate::api::schema::AgentInfo,
            std::sync::mpsc::Receiver<std::io::Result<()>>,
        ),
        String,
    > {
        if params.text.is_empty() {
            return Err(encode_error(
                id,
                "empty_agent_prompt",
                "agent prompt must not be empty",
            ));
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(encode_error_body(id, self.agent_target_error_body(err))),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return Err(agent_not_found(id, &params.target));
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        if terminal.state == crate::detect::AgentState::Blocked {
            return Err(encode_error(
                id,
                "agent_blocked",
                format!(
                    "agent {} is blocked and requires interactive input",
                    params.target
                ),
            ));
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return Err(agent_not_ready(id, &params.target));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(agent_not_ready(id, &params.target));
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!(
                    "agent {} is no longer the pane foreground process",
                    params.target
                ),
            ));
        }
        #[cfg(windows)]
        let submit_deadline = params
            .wait
            .as_ref()
            .and_then(|wait| wait.submission_deadline);
        #[cfg(not(windows))]
        let submit_deadline = None;
        if expected_agent == crate::detect::Agent::GithubCopilot {
            // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
            let focus = match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => focus,
                Err(err) => {
                    return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
                }
            };
            if let Err(err) = runtime.try_send_bytes(Bytes::from(focus)) {
                return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
            }
        }
        let (text, enter) =
            crate::app::api_helpers::encode_api_submission_parts(runtime, &params.text);
        #[cfg(windows)]
        let text = if expected_agent == crate::detect::Agent::Codex {
            let mut text = text;
            append_codex_paste_boundary(runtime, &mut text);
            text
        } else {
            text
        };
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        let completion = runtime
            .queue_user_input_submission(
                Bytes::from(text),
                Bytes::from(enter),
                AGENT_PROMPT_SUBMIT_DELAY,
                submit_deadline,
            )
            .map_err(|err| encode_error(id.clone(), "agent_prompt_failed", err.to_string()))?;
        Ok((id, agent, completion))
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            pane,
            params.source,
            params.format,
            params.lines,
        );

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    format: params.format,
                    text: snapshot.text,
                    revision: 0,
                    truncated: snapshot.truncated,
                },
            },
        )
    }

    pub(super) fn handle_agent_explain(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(id, &target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": crate::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "manifest_version": null,
                "cached_remote_version": null,
                "local_override_shadowing_remote": false,
                "remote_update_status": null,
                "remote_update_error": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return encode_success(id, ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return encode_error(
                id,
                "agent_explain_unavailable",
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = crate::detect::manifest::explain_to_json_value(&explain);

        encode_success(id, ResponseResult::AgentExplain { explain: value })
    }

    pub(super) fn handle_agent_send_keys(
        &mut self,
        id: String,
        params: AgentSendKeysParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.effective_known_agent())
        else {
            return agent_not_ready(id, &params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(id, &params.target);
        }
        let encoded = match super::super::api_helpers::encode_api_keys(runtime, &params.keys) {
            Ok(encoded) => encoded,
            Err(key) => {
                return encode_error(id, "invalid_key", format!("unsupported key {key}"));
            }
        };
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            return encode_error(id, "agent_send_keys_failed", err.to_string());
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

fn agent_not_ready(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_ready",
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{AgentStatus, SuccessResponse},
        app::Mode,
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    fn app_with_agent_and_events() -> (App, crate::api::EventHub) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let event_hub = crate::api::EventHub::default();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            event_hub.clone(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        (app, event_hub)
    }

    fn app_with_agent() -> App {
        app_with_agent_and_events().0
    }

    fn start_deferred_agent_prompt(
        app: &mut App,
        id: &str,
        params: AgentPromptParams,
    ) -> std::sync::mpsc::Receiver<String> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: id.into(),
                method: crate::api::schema::Method::AgentPrompt(params),
            },
            respond_to,
        ));
        response_rx
    }

    fn run_deferred_agent_prompt(app: &mut App, id: &str, params: AgentPromptParams) -> String {
        start_deferred_agent_prompt(app, id, params)
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission")
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_codex_prompt_flushes_paste_burst_before_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        // The non-character key must precede Enter so Codex commits the paste burst first.
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B\x1b[C"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn a_false_process_exit_makes_a_named_live_agent_unreachable_by_name() {
        // Reproduces the registration loss reported on #3225 by rszrszrsz:
        // a live agent pane with an assigned name stops resolving by that name
        // while its process keeps running, and renaming is the only recovery.
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let observed_at = std::time::Instant::now();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
        terminal.set_agent_name("reviewer".into());

        let found = app.handle_agent_get(
            "req:before".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&found).is_ok(),
            "the assigned name must resolve while the agent is running: {found}"
        );

        // One process-exit observation, then the same agent is observed alive
        // again on the next probe - the process never actually went away.
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at,
        });
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            observed_at: observed_at + std::time::Duration::from_secs(1),
        });

        let terminal = &app.state.terminals[&terminal_id];
        assert_eq!(
            terminal.detected_agent,
            Some(Agent::Pi),
            "the agent process is still there"
        );

        let after = app.handle_agent_get(
            "req:after".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&after).is_ok(),
            "a live agent must stay reachable by its assigned name: {after}"
        );
    }

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 2,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let bracketed_started = std::time::Instant::now();
        let response_rx = start_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: public_pane_id,
                text: "A != B".into(),
                wait: None,
            },
        );
        assert!(response_rx.try_recv().is_err());
        let response = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw = run_deferred_agent_prompt(
            &mut app,
            "req-raw",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let raw: SuccessResponse = serde_json::from_str(&raw).unwrap();
        assert!(matches!(raw.result, ResponseResult::AgentPrompted { .. }));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = run_deferred_agent_prompt(
            &mut app,
            "req-label",
            AgentPromptParams {
                target: "opencode".into(),
                text: "wrong target".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "unrelated prompt".into(),
                wait: None,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 3,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(
            "req-valid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["up".into(), "enter".into()],
            },
        );
        let success: SuccessResponse = serde_json::from_str(&sent).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[A\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req-pending",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_focus_marks_already_focused_done_agent_seen() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);

        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        app.state.workspaces[0].tabs[0].layout.focus_pane(pane_id);

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, pane_id).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Idle);
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(
                "req".into(),
                AgentRenameParams {
                    target: target.clone(),
                    name,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }

    // -----------------------------------------------------------------------
    // agent.acknowledge
    // -----------------------------------------------------------------------

    /// Two workspaces with the agent in the background tab. That is the case a background
    /// acknowledgement exists for: the completion stays unacknowledged (Done) without focus.
    fn app_with_background_pane() -> (App, crate::layout::PaneId, crate::api::EventHub) {
        let (mut app, event_hub) = app_with_agent_and_events();
        app.state.workspaces = vec![
            Workspace::test_new("active"),
            Workspace::test_new("background"),
        ];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let pane_id = app.state.workspaces[1].tabs[0].root_pane;
        (app, pane_id, event_hub)
    }

    fn app_with_background_completion() -> (App, crate::layout::PaneId, crate::api::EventHub) {
        let (mut app, pane_id, event_hub) = app_with_background_pane();
        record_completion(&mut app, pane_id);
        (app, pane_id, event_hub)
    }

    fn record_completion(app: &mut App, pane_id: crate::layout::PaneId) {
        for state in [AgentState::Working, AgentState::Idle] {
            record_state(app, pane_id, state);
        }
    }

    fn record_state(app: &mut App, pane_id: crate::layout::PaneId, state: AgentState) {
        let updates = app
            .state
            .handle_app_event(crate::events::AppEvent::StateChanged {
                pane_id,
                agent: Some(Agent::Pi),
                state,
                visible_blocker: false,
                visible_working: state == AgentState::Working,
                process_exited: false,
                observed_at: std::time::Instant::now(),
            });
        for update in &updates {
            app.emit_pane_state_update(update);
        }
    }

    /// The projection a caller reads its token from, exactly as `agent get --json` returns it.
    fn agent_token(
        app: &App,
        pane_id: crate::layout::PaneId,
    ) -> crate::api::schema::AgentAcknowledgeParams {
        let agent = app
            .agent_info(1, pane_id)
            .expect("background pane hosts an agent");
        crate::api::schema::AgentAcknowledgeParams {
            target: agent.pane_id.clone(),
            terminal_id: agent.terminal_id,
            completion_seq: agent.completion_seq.unwrap_or(agent.state_change_seq),
            agent_session_value: agent.agent_session.map(|session| session.value),
        }
    }

    fn acknowledge(
        app: &mut App,
        params: crate::api::schema::AgentAcknowledgeParams,
    ) -> serde_json::Value {
        serde_json::from_str(&app.handle_agent_acknowledge("req_ack".into(), params)).unwrap()
    }

    fn acknowledged(
        response: serde_json::Value,
    ) -> (
        crate::api::schema::AgentInfo,
        crate::api::schema::AgentAcknowledgeOutcome,
    ) {
        let success: SuccessResponse = serde_json::from_value(response).unwrap();
        let ResponseResult::AgentCompletionAcknowledged { agent, outcome } = success.result else {
            panic!("expected an acknowledged completion result");
        };
        (agent, outcome)
    }

    fn rejected(response: serde_json::Value) -> String {
        let error: crate::api::schema::ErrorResponse = serde_json::from_value(response).unwrap();
        error.error.code
    }

    fn background_terminal(
        app: &App,
        pane_id: crate::layout::PaneId,
    ) -> &crate::terminal::TerminalState {
        let terminal_id = &app.state.workspaces[1].tabs[0].panes[&pane_id].attached_terminal_id;
        app.state
            .terminals
            .get(terminal_id)
            .expect("background terminal exists")
    }

    fn background_seen(app: &App, pane_id: crate::layout::PaneId) -> bool {
        app.state.workspaces[1]
            .pane_state(pane_id)
            .expect("background pane exists")
            .seen
    }

    fn set_reported_session(app: &mut App, pane_id: crate::layout::PaneId, value: &str) {
        let terminal_id = app.state.workspaces[1].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("background terminal exists");
        terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            session_ref: crate::agent_resume::AgentSessionRef {
                kind: crate::agent_resume::AgentSessionRefKind::Id,
                value: value.to_string(),
            },
        });
    }

    /// Same `micros`/`counter` tail with a different process epoch: what an earlier server boot
    /// mints for the same pane id.
    fn foreign_boot_terminal_id(terminal_id: &str) -> String {
        let parts: Vec<&str> = terminal_id.split('_').collect();
        assert_eq!(
            parts.len(),
            5,
            "unexpected terminal id shape: {terminal_id}"
        );
        format!("term_ffffffff_ffffffff_{}_{}", parts[3], parts[4])
    }

    #[test]
    fn completion_ack_normal_clears_completion_and_clears_done() {
        let (mut app, pane_id, _event_hub) = app_with_background_completion();
        assert_eq!(
            app.agent_info(1, pane_id).unwrap().agent_status,
            AgentStatus::Done
        );
        let token = agent_token(&app, pane_id);
        let seq = token.completion_seq;

        let (agent, outcome) = acknowledged(acknowledge(&mut app, token));

        assert_eq!(
            outcome,
            crate::api::schema::AgentAcknowledgeOutcome::Acknowledged
        );
        assert_eq!(agent.agent_status, AgentStatus::Idle);
        assert_eq!(agent.completion_seq, None);
        let terminal = background_terminal(&app, pane_id);
        assert_eq!(terminal.last_agent_completion_seq, None);
        assert_eq!(terminal.last_acked_agent_completion, Some(seq));
        assert!(background_seen(&app, pane_id));
        assert_eq!(
            app.agent_info(1, pane_id).unwrap().agent_status,
            AgentStatus::Idle
        );
    }

    #[test]
    fn completion_ack_repeat_is_idempotent() {
        let (mut app, pane_id, event_hub) = app_with_background_completion();
        let token = agent_token(&app, pane_id);
        let seq = token.completion_seq;
        let (_, first) = acknowledged(acknowledge(&mut app, token.clone()));
        assert_eq!(
            first,
            crate::api::schema::AgentAcknowledgeOutcome::Acknowledged
        );

        let before = event_hub.current_sequence();
        let (agent, second) = acknowledged(acknowledge(&mut app, token));

        assert_eq!(
            second,
            crate::api::schema::AgentAcknowledgeOutcome::AlreadyAcknowledged
        );
        assert_eq!(agent.agent_status, AgentStatus::Idle);
        let terminal = background_terminal(&app, pane_id);
        assert_eq!(terminal.last_agent_completion_seq, None);
        assert_eq!(terminal.last_acked_agent_completion, Some(seq));
        assert!(background_seen(&app, pane_id));
        assert!(
            event_hub.events_after(before).is_empty(),
            "a repeated acknowledgement must not publish another event"
        );
    }

    #[test]
    fn completion_ack_rejects_old_sequence() {
        let (mut app, pane_id, event_hub) = app_with_background_completion();
        let stale = agent_token(&app, pane_id);
        record_completion(&mut app, pane_id);
        let fresh = agent_token(&app, pane_id);
        assert_ne!(stale.completion_seq, fresh.completion_seq);
        let before = event_hub.current_sequence();

        let code = rejected(acknowledge(&mut app, stale));

        assert_eq!(code, "agent_completion_stale");
        let terminal = background_terminal(&app, pane_id);
        assert_eq!(
            terminal.last_agent_completion_seq,
            Some(fresh.completion_seq)
        );
        assert_eq!(terminal.last_acked_agent_completion, None);
        assert!(!background_seen(&app, pane_id));
        assert_eq!(
            app.agent_info(1, pane_id).unwrap().agent_status,
            AgentStatus::Done
        );
        assert!(event_hub.events_after(before).is_empty());
    }

    #[test]
    fn completion_ack_rejects_foreign_terminal() {
        let (mut app, pane_id, event_hub) = app_with_background_completion();
        let token = agent_token(&app, pane_id);
        let before = event_hub.current_sequence();

        let mut foreign = token.clone();
        foreign.terminal_id = foreign_boot_terminal_id(&token.terminal_id);
        assert_ne!(foreign.terminal_id, token.terminal_id);
        assert_eq!(
            rejected(acknowledge(&mut app, foreign)),
            "agent_completion_stale"
        );

        let mut unknown = token.clone();
        unknown.terminal_id = "term_0_0_0_0".into();
        assert_eq!(
            rejected(acknowledge(&mut app, unknown)),
            "agent_completion_stale"
        );

        let terminal = background_terminal(&app, pane_id);
        assert_eq!(
            terminal.last_agent_completion_seq,
            Some(token.completion_seq)
        );
        assert_eq!(terminal.last_acked_agent_completion, None);
        assert!(!background_seen(&app, pane_id));
        assert!(event_hub.events_after(before).is_empty());
    }

    #[test]
    fn completion_ack_rejects_replaced_agent_session() {
        let (mut app, pane_id, event_hub) = app_with_background_completion();
        set_reported_session(&mut app, pane_id, "session-a");
        let token = agent_token(&app, pane_id);
        assert_eq!(token.agent_session_value.as_deref(), Some("session-a"));
        set_reported_session(&mut app, pane_id, "session-b");
        assert_eq!(
            app.agent_info(1, pane_id)
                .unwrap()
                .agent_session
                .map(|session| session.value)
                .as_deref(),
            Some("session-b")
        );
        let before = event_hub.current_sequence();

        let code = rejected(acknowledge(&mut app, token));

        assert_eq!(code, "agent_completion_stale");
        assert_eq!(
            background_terminal(&app, pane_id).last_acked_agent_completion,
            None
        );
        assert!(event_hub.events_after(before).is_empty());
    }

    #[test]
    fn completion_ack_does_not_touch_working_or_blocked() {
        for blocked in [false, true] {
            let (mut app, pane_id, event_hub) = app_with_background_pane();
            record_state(&mut app, pane_id, AgentState::Working);
            if blocked {
                record_state(&mut app, pane_id, AgentState::Blocked);
            }
            let expected = if blocked {
                AgentState::Blocked
            } else {
                AgentState::Working
            };
            let expected_status = if blocked {
                AgentStatus::Blocked
            } else {
                AgentStatus::Working
            };
            let before = event_hub.current_sequence();
            let mut pending_before: Vec<_> = app
                .state
                .pending_agent_notifications
                .keys()
                .copied()
                .collect();
            pending_before.sort_by_key(|pane| pane.raw());

            let mut mismatched = agent_token(&app, pane_id);
            assert_eq!(
                rejected(acknowledge(&mut app, mismatched.clone())),
                "agent_completion_absent"
            );
            mismatched.completion_seq += 1;
            assert_eq!(
                rejected(acknowledge(&mut app, mismatched)),
                "agent_completion_stale"
            );

            let terminal = background_terminal(&app, pane_id);
            assert_eq!(terminal.state, expected);
            assert_eq!(terminal.last_agent_completion_seq, None);
            assert_eq!(terminal.last_acked_agent_completion, None);
            assert_eq!(
                app.agent_info(1, pane_id).unwrap().agent_status,
                expected_status
            );
            let mut pending_after: Vec<_> = app
                .state
                .pending_agent_notifications
                .keys()
                .copied()
                .collect();
            pending_after.sort_by_key(|pane| pane.raw());
            assert_eq!(pending_before, pending_after);
            assert!(event_hub.events_after(before).is_empty());
        }
    }

    #[test]
    fn completion_ack_without_completion_is_absent() {
        // Unknown -> Idle moves the state sequence without recording a completion, so a matching
        // sequence must still report that there is nothing to acknowledge.
        let (mut app, pane_id, event_hub) = app_with_background_pane();
        record_state(&mut app, pane_id, AgentState::Idle);
        let terminal = background_terminal(&app, pane_id);
        assert_eq!(terminal.last_agent_completion_seq, None);
        let seen_before = background_seen(&app, pane_id);
        let before = event_hub.current_sequence();

        let token = agent_token(&app, pane_id);
        let code = rejected(acknowledge(&mut app, token));

        assert_eq!(code, "agent_completion_absent");
        assert_eq!(background_seen(&app, pane_id), seen_before);
        assert!(event_hub.events_after(before).is_empty());
    }

    #[test]
    fn completion_ack_missing_pane_or_agent_is_not_found() {
        let (mut app, _pane_id, _event_hub) = app_with_background_pane();
        let shell_pane = app.state.workspaces[0].tabs[0].root_pane;
        let shell_target = app
            .public_pane_id(0, shell_pane)
            .expect("active shell pane has a public id");

        for target in [shell_target, "no-such-agent".to_string()] {
            let code = rejected(acknowledge(
                &mut app,
                crate::api::schema::AgentAcknowledgeParams {
                    target,
                    terminal_id: "term_0_0_0_0".into(),
                    completion_seq: 1,
                    agent_session_value: None,
                },
            ));
            assert_eq!(code, "agent_not_found");
        }
    }

    #[test]
    fn completion_ack_interleaved_with_new_completion() {
        let (mut app, pane_id, _event_hub) = app_with_background_completion();
        let stale = agent_token(&app, pane_id);
        record_completion(&mut app, pane_id);
        let fresh = agent_token(&app, pane_id);
        assert_ne!(stale.completion_seq, fresh.completion_seq);

        assert_eq!(
            rejected(acknowledge(&mut app, stale)),
            "agent_completion_stale"
        );
        let (agent, outcome) = acknowledged(acknowledge(&mut app, fresh.clone()));

        assert_eq!(
            outcome,
            crate::api::schema::AgentAcknowledgeOutcome::Acknowledged
        );
        assert_eq!(agent.completion_seq, None);
        assert_eq!(
            background_terminal(&app, pane_id).last_acked_agent_completion,
            Some(fresh.completion_seq)
        );
    }

    #[test]
    fn completion_ack_preserves_focus_workspace_tab_and_identity() {
        let (mut app, pane_id, _event_hub) = app_with_background_completion();
        let before_agent = app.agent_info(1, pane_id).unwrap();
        let before_state = (
            app.state.active,
            app.state.selected,
            app.state.mode,
            app.state.workspaces[0].id.clone(),
            app.state.workspaces[1].id.clone(),
            app.state.workspaces[1].active_tab,
            app.state.workspaces[1].focused_pane_id(),
            app.state.workspaces[1].tabs[0].number,
        );
        let before_identity = (
            before_agent.terminal_id.clone(),
            before_agent.workspace_id.clone(),
            before_agent.tab_id.clone(),
            before_agent.pane_id.clone(),
            before_agent.name.clone(),
            before_agent.agent.clone(),
            before_agent.agent_session.clone(),
            before_agent.revision,
            before_agent.state_change_seq,
        );
        let token = agent_token(&app, pane_id);

        let (_, outcome) = acknowledged(acknowledge(&mut app, token));

        assert_eq!(
            outcome,
            crate::api::schema::AgentAcknowledgeOutcome::Acknowledged
        );
        let after_agent = app.agent_info(1, pane_id).unwrap();
        let after_state = (
            app.state.active,
            app.state.selected,
            app.state.mode,
            app.state.workspaces[0].id.clone(),
            app.state.workspaces[1].id.clone(),
            app.state.workspaces[1].active_tab,
            app.state.workspaces[1].focused_pane_id(),
            app.state.workspaces[1].tabs[0].number,
        );
        let after_identity = (
            after_agent.terminal_id.clone(),
            after_agent.workspace_id.clone(),
            after_agent.tab_id.clone(),
            after_agent.pane_id.clone(),
            after_agent.name.clone(),
            after_agent.agent.clone(),
            after_agent.agent_session.clone(),
            after_agent.revision,
            after_agent.state_change_seq,
        );
        assert_eq!(before_state, after_state);
        assert_eq!(before_identity, after_identity);
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn agent_completion_acknowledged_response_round_trips() {
        let (mut app, pane_id, _event_hub) = app_with_background_completion();
        let token = agent_token(&app, pane_id);

        let response = app.handle_agent_acknowledge("req_round_trip".into(), token);

        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            parsed.result,
            ResponseResult::AgentCompletionAcknowledged { .. }
        ));
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["result"]["type"], "agent_completion_acknowledged");
        assert_eq!(value["result"]["outcome"], "acknowledged");
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            value,
            "the acknowledged response must re-encode to the same wire shape"
        );
    }

    #[test]
    fn completion_ack_emits_status_change_event_only() {
        let (mut app, pane_id, event_hub) = app_with_background_completion();
        let token = agent_token(&app, pane_id);
        let before = event_hub.current_sequence();

        acknowledged(acknowledge(&mut app, token));

        let events = event_hub.events_after(before);
        assert_eq!(events.len(), 1, "unexpected events: {events:?}");
        assert_eq!(
            events[0].1.event,
            crate::api::schema::EventKind::PaneAgentStatusChanged
        );
        let crate::api::schema::EventData::PaneAgentStatusChanged { agent_status, .. } =
            &events[0].1.data
        else {
            panic!("expected a pane agent status change payload");
        };
        assert_eq!(*agent_status, AgentStatus::Idle);
    }
}
