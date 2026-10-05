//! Real-binary integration coverage for `agent.acknowledge`.
//!
//! Every case drives an isolated server (its own API socket, its own XDG roots) plus the real
//! `herdr` CLI, so the acknowledgement path is exercised the way an external caller uses it.
//! Nothing here focuses a pane, tab, or agent: clearing a completion must never need focus, and
//! this file must stay free of focus requests so the property is actually proven.

#![cfg(unix)]

pub mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use support::{
    cleanup_test_base, client_shell_handshake, decode_string, register_runtime_dir,
    register_spawned_herdr_pid, unregister_spawned_herdr_pid, CURRENT_ENDPOINT_PROTOCOL_GENERATION,
    SERVER_MESSAGE_ENDPOINT_CONTROL,
};

/// Companion kind carrying the per-pane unacknowledged completion map.
const AGENT_COMPLETIONS_KIND: &str = "endpoint.agent-completions.v1";

/// Ambient Herdr context this test must never let leak into the server or CLI it spawns.
const INHERITED_HERDR_VARIABLES: &[&str] = &[
    "HERDR_ENV",
    "HERDR_CLIENT_SOCKET_PATH",
    "HERDR_STARTUP_CWD",
    "HERDR_SESSION",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_PANE_ID",
];

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-completion-ack-test-{}-{nanos}",
        std::process::id()
    ))
}

struct SpawnedServer {
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedServer {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        drop(self._master.take());

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

struct TestServer {
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    socket_path: PathBuf,
    client_socket_path: PathBuf,
    spawned: Option<SpawnedServer>,
}

impl TestServer {
    /// Start a real server whose sockets and XDG roots are private to this test.
    fn start() -> Self {
        let base = unique_test_dir();
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let socket_path = runtime_dir.join("herdr.sock");
        let client_socket_path = runtime_dir.join("herdr-client.sock");
        std::fs::create_dir_all(config_home.join("herdr")).unwrap();
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(
            config_home.join("herdr/config.toml"),
            "onboarding = false\n",
        )
        .unwrap();
        register_runtime_dir(&runtime_dir);

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
        command.arg("server");
        // Same inherited-variable isolation as the CLI subprocess below, so neither child can pick
        // up the ambient Herdr session this test itself may be running inside.
        for variable in INHERITED_HERDR_VARIABLES {
            command.env_remove(variable);
        }
        command.env("XDG_STATE_HOME", runtime_dir.join("state"));
        command.env("XDG_CONFIG_HOME", &config_home);
        command.env("XDG_RUNTIME_DIR", &runtime_dir);
        command.env("HERDR_SOCKET_PATH", &socket_path);
        command.env("SHELL", "/bin/sh");
        let child = pair.slave.spawn_command(command).unwrap();
        register_spawned_herdr_pid(child.process_id());
        drop(pair.slave);

        let server = TestServer {
            base,
            config_home,
            runtime_dir,
            socket_path,
            client_socket_path,
            spawned: Some(SpawnedServer {
                _master: Some(pair.master),
                child,
            }),
        };
        wait_for_socket(&server.socket_path, Duration::from_secs(10));
        wait_for_socket(&server.client_socket_path, Duration::from_secs(10));
        server
    }

    fn request(&self, id: &str, method: &str, params: Value) -> Value {
        send_json_request(&self.socket_path, id, method, params)
    }

    /// Run the real CLI against this server's isolated socket.
    fn cli(&self, args: &[&str]) -> (i32, String, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command.args(args);
        for variable in INHERITED_HERDR_VARIABLES {
            command.env_remove(variable);
        }
        let output = command
            .env("XDG_STATE_HOME", self.runtime_dir.join("state"))
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("HERDR_SOCKET_PATH", &self.socket_path)
            .output()
            .expect("herdr CLI runs");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        drop(self.spawned.take());
        cleanup_test_base(&self.base);
    }
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("api socket did not appear at {}", path.display());
}

fn send_json_request(socket_path: &Path, id: &str, method: &str, params: Value) -> Value {
    use std::io::{BufRead, BufReader, Write as _};
    let mut stream =
        std::os::unix::net::UnixStream::connect(socket_path).expect("connect to the API socket");
    let request = json!({ "id": id, "method": method, "params": params });
    writeln!(stream, "{request}").unwrap();
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).expect("API response is JSON")
}

/// One client-shell connection that only observes the agent-completion companion.
struct ShellClient {
    stream: std::os::unix::net::UnixStream,
}

impl ShellClient {
    fn connect(socket_path: &Path) -> Self {
        let mut stream =
            std::os::unix::net::UnixStream::connect(socket_path).expect("client shell connects");
        client_shell_handshake(&mut stream, CURRENT_ENDPOINT_PROTOCOL_GENERATION, 80, 24)
            .expect("client shell handshake");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        Self { stream }
    }

    /// Read companions until one newer than `after_revision` satisfies `predicate`.
    fn wait_for_completions(
        &mut self,
        after_revision: u64,
        predicate: impl Fn(&[String]) -> bool,
        timeout: Duration,
    ) -> (u64, Vec<String>) {
        let deadline = Instant::now() + timeout;
        let mut last: Option<(u64, Vec<String>)> = None;
        while Instant::now() < deadline {
            let Ok((variant, payload)) = support::read_server_message(&mut self.stream) else {
                continue;
            };
            if variant != SERVER_MESSAGE_ENDPOINT_CONTROL {
                continue;
            }
            let Some((revision, panes)) = decode_companion(&payload) else {
                continue;
            };
            if revision <= after_revision {
                continue;
            }
            if predicate(&panes) {
                return (revision, panes);
            }
            last = Some((revision, panes));
        }
        panic!("no matching agent-completion companion within the timeout; last seen: {last:?}");
    }
}

fn decode_companion(payload: &[u8]) -> Option<(u64, Vec<String>)> {
    let mut offset = 0;
    if decode_string(payload, &mut offset).ok()? != AGENT_COMPLETIONS_KIND {
        return None;
    }
    let data = decode_string(payload, &mut offset).ok()?;
    let value: Value = serde_json::from_str(&data).ok()?;
    let revision = value["revision"].as_u64()?;
    let panes = value["completions"]
        .as_object()
        .map(|panes| panes.keys().cloned().collect())
        .unwrap_or_default();
    Some((revision, panes))
}

fn contains_pane(panes: &[String], pane: &str) -> bool {
    panes.iter().any(|candidate| candidate == pane)
}

/// Two workspaces with an agent-capable pane in the background one. Creating the workspaces does
/// not report any agent activity, so callers can attach observers before work happens.
struct Fixture {
    server: TestServer,
    agent_pane: String,
}

/// The acknowledgement token a caller reads once, from a single `agent get` response.
struct CompletionToken {
    terminal_id: String,
    session_value: Option<String>,
    completion_seq: u64,
}

fn fixture_with_background_pane() -> Fixture {
    let server = TestServer::start();

    let active = server.request(
        "setup_active",
        "workspace.create",
        json!({ "cwd": server.base.join("active").display().to_string() }),
    );
    assert_eq!(active["result"]["type"], "workspace_created", "{active}");
    let background = server.request(
        "setup_background",
        "workspace.create",
        json!({ "cwd": server.base.join("background").display().to_string() }),
    );
    assert_eq!(
        background["result"]["type"], "workspace_created",
        "{background}"
    );
    let agent_pane = background["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("created workspace exposes its root pane")
        .to_string();

    Fixture { server, agent_pane }
}

/// Drive the background agent through `Working` -> `Idle` so a completion is recorded without any
/// focus happening.
fn report_completion(fixture: &Fixture) {
    for (seq, state) in [(1_u64, "working"), (2, "idle")] {
        let reported = fixture.server.request(
            &format!("setup_report_{seq}"),
            "pane.report_agent",
            json!({
                "pane_id": fixture.agent_pane,
                // An official source yields the agent-session part of the token; `codex` is official
                // without being a reserved native state source, so it keeps hook state authority.
                "source": "herdr:codex",
                "agent": "codex",
                "state": state,
                "seq": seq,
                "agent_session_id": "completion-ack-session",
            }),
        );
        assert_eq!(reported["result"]["type"], "ok", "{reported}");
    }
}

fn read_token(server: &TestServer, pane_id: &str) -> CompletionToken {
    let agents = server.request("read_token", "agent.list", json!({}));
    let agent = agents["result"]["agents"]
        .as_array()
        .expect("agent list")
        .first()
        .cloned()
        .expect("the reported agent is listed");
    assert_eq!(agent["pane_id"].as_str(), Some(pane_id));
    CompletionToken {
        terminal_id: agent["terminal_id"]
            .as_str()
            .expect("agent exposes its terminal id")
            .to_string(),
        session_value: agent["agent_session"]["value"].as_str().map(str::to_string),
        completion_seq: agent["completion_seq"]
            .as_u64()
            .expect("a completion is recorded"),
    }
}

fn acknowledge_args(pane_id: &str, token: &CompletionToken) -> Vec<String> {
    let mut args = vec![
        "agent".to_string(),
        "acknowledge".to_string(),
        pane_id.to_string(),
        "--completion-seq".to_string(),
        token.completion_seq.to_string(),
        "--terminal-id".to_string(),
        token.terminal_id.clone(),
    ];
    if let Some(session) = token.session_value.as_deref() {
        args.push("--session-value".to_string());
        args.push(session.to_string());
    }
    args
}

/// The navigation projection an acknowledgement must not move.
///
/// The three ids are asserted to be real, non-empty public ids before they are compared, so a
/// before/after comparison cannot pass on three identical nulls, and the focused pane is asserted
/// to be something other than the background agent pane this test acknowledges.
fn focus_projection(server: &TestServer, background_pane: &str) -> Value {
    let response = server.request("focus_projection", "session.snapshot", json!({}));
    assert_eq!(
        response["result"]["type"], "session_snapshot",
        "session.snapshot must succeed: {response}"
    );
    let snapshot = response["result"]["snapshot"]
        .as_object()
        .expect("session.snapshot returns a snapshot object");
    for field in ["focused_workspace_id", "focused_tab_id", "focused_pane_id"] {
        let value = snapshot.get(field).and_then(Value::as_str);
        assert!(
            value.is_some_and(|value| !value.is_empty()),
            "{field} must be a real non-empty id, got {:?}",
            snapshot.get(field)
        );
    }
    let focused_pane = snapshot["focused_pane_id"]
        .as_str()
        .expect("validated above");
    assert_ne!(
        focused_pane, background_pane,
        "the fixture's focused pane must not be the background agent pane"
    );
    json!({
        "focused_workspace_id": snapshot["focused_workspace_id"],
        "focused_tab_id": snapshot["focused_tab_id"],
        "focused_pane_id": snapshot["focused_pane_id"],
    })
}

fn agent_field(server: &TestServer, id: &str, field: &str) -> Value {
    let agents = server.request(id, "agent.list", json!({}));
    agents["result"]["agents"][0][field].clone()
}

#[test]
fn completion_ack_cli_clears_done_without_focus() {
    let fixture = fixture_with_background_pane();
    let server = &fixture.server;
    let pane = fixture.agent_pane.clone();
    let focus_before = focus_projection(server, &fixture.agent_pane);

    let mut first_client = ShellClient::connect(&server.client_socket_path);
    let mut second_client = ShellClient::connect(&server.client_socket_path);
    let (first_revision, first_initial) =
        first_client.wait_for_completions(0, |_| true, Duration::from_secs(10));
    let (second_revision, second_initial) =
        second_client.wait_for_completions(0, |_| true, Duration::from_secs(10));
    assert!(
        !contains_pane(&first_initial, &pane),
        "no completion before the agent reports work: {first_initial:?}"
    );
    assert!(
        !contains_pane(&second_initial, &pane),
        "no completion before the agent reports work: {second_initial:?}"
    );

    report_completion(&fixture);
    assert_eq!(
        agent_field(server, "setup_status", "agent_status"),
        json!("done")
    );

    let (first_completed_revision, first_completed) = first_client.wait_for_completions(
        first_revision,
        |panes| contains_pane(panes, &pane),
        Duration::from_secs(10),
    );
    let (second_completed_revision, second_completed) = second_client.wait_for_completions(
        second_revision,
        |panes| contains_pane(panes, &pane),
        Duration::from_secs(10),
    );
    assert!(first_completed_revision > first_revision);
    assert!(second_completed_revision > second_revision);
    assert!(contains_pane(&first_completed, &pane));
    assert!(contains_pane(&second_completed, &pane));

    let token = read_token(server, &pane);
    let args = acknowledge_args(&pane, &token);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = server.cli(&arg_refs);

    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert!(stderr.is_empty(), "stderr={stderr}");
    let response: Value = serde_json::from_str(&stdout).expect("acknowledge prints JSON");
    assert_eq!(
        response["result"]["type"], "agent_completion_acknowledged",
        "{response}"
    );
    assert_eq!(response["result"]["outcome"], "acknowledged", "{response}");
    assert_eq!(response["result"]["agent"]["agent_status"], "idle");
    assert_eq!(response["result"]["agent"]["completion_seq"], Value::Null);
    // The session identity the caller read is part of the token and must survive the round trip.
    let session = token
        .session_value
        .as_deref()
        .expect("the acknowledgement token carries the reported agent session");
    assert_eq!(
        response["result"]["agent"]["agent_session"]["value"], session,
        "{response}"
    );

    assert_eq!(
        agent_field(server, "after_completion_seq", "completion_seq"),
        Value::Null
    );
    assert_eq!(
        agent_field(server, "after_status", "agent_status"),
        json!("idle")
    );
    assert_eq!(
        focus_projection(server, &fixture.agent_pane),
        focus_before,
        "an acknowledgement must not move focus, tab, or workspace"
    );

    let (first_acked_revision, first_acked) = first_client.wait_for_completions(
        first_completed_revision,
        |panes| !contains_pane(panes, &pane),
        Duration::from_secs(10),
    );
    let (second_acked_revision, second_acked) = second_client.wait_for_completions(
        second_completed_revision,
        |panes| !contains_pane(panes, &pane),
        Duration::from_secs(10),
    );
    assert!(first_acked_revision > first_completed_revision);
    assert!(second_acked_revision > second_completed_revision);
    assert!(!contains_pane(&first_acked, &pane), "{first_acked:?}");
    assert!(!contains_pane(&second_acked, &pane), "{second_acked:?}");

    let (repeat_code, repeat_stdout, repeat_stderr) = server.cli(&arg_refs);
    assert_eq!(repeat_code, 0, "stderr={repeat_stderr}");
    let repeat: Value = serde_json::from_str(&repeat_stdout).expect("repeat prints JSON");
    assert_eq!(
        repeat["result"]["outcome"], "already_acknowledged",
        "{repeat}"
    );
    assert_eq!(
        focus_projection(server, &fixture.agent_pane),
        focus_before,
        "a repeated acknowledgement must not move focus either"
    );
}

#[test]
fn completion_ack_cli_rejects_stale_tokens_and_usage_errors() {
    let fixture = fixture_with_background_pane();
    let server = &fixture.server;
    report_completion(&fixture);
    let token = read_token(server, &fixture.agent_pane);

    let stale_seq = (token.completion_seq + 1).to_string();
    let completion_seq = token.completion_seq.to_string();
    let (code, stdout, stderr) = server.cli(&[
        "agent",
        "acknowledge",
        fixture.agent_pane.as_str(),
        "--completion-seq",
        stale_seq.as_str(),
        "--terminal-id",
        token.terminal_id.as_str(),
    ]);
    assert_eq!(code, 1, "stdout={stdout} stderr={stderr}");
    let error: Value = serde_json::from_str(&stderr).expect("stale acknowledgement prints JSON");
    assert_eq!(error["error"]["code"], "agent_completion_stale", "{error}");

    let (code, stdout, stderr) = server.cli(&[
        "agent",
        "acknowledge",
        fixture.agent_pane.as_str(),
        "--completion-seq",
        completion_seq.as_str(),
        "--terminal-id",
        "term_0_0_0_0",
    ]);
    assert_eq!(code, 1, "stdout={stdout} stderr={stderr}");
    let error: Value = serde_json::from_str(&stderr).expect("foreign terminal prints JSON");
    assert_eq!(error["error"]["code"], "agent_completion_stale", "{error}");

    let (code, stdout, stderr) = server.cli(&[
        "agent",
        "acknowledge",
        "no-such-agent",
        "--completion-seq",
        completion_seq.as_str(),
        "--terminal-id",
        token.terminal_id.as_str(),
    ]);
    assert_eq!(code, 1, "stdout={stdout} stderr={stderr}");
    let error: Value = serde_json::from_str(&stderr).expect("unknown target prints JSON");
    assert_eq!(error["error"]["code"], "agent_not_found", "{error}");

    // A caller that omits the terminal identity gets a usage error instead of an unbound clear.
    let (code, _stdout, stderr) = server.cli(&[
        "agent",
        "acknowledge",
        fixture.agent_pane.as_str(),
        "--completion-seq",
        completion_seq.as_str(),
    ]);
    assert_eq!(code, 2, "stderr={stderr}");
    assert!(stderr.contains("--terminal-id"), "stderr={stderr}");

    // Everything above was rejected, so the recorded completion must still be intact.
    assert_eq!(
        agent_field(server, "still_recorded", "completion_seq"),
        json!(token.completion_seq)
    );
    assert_eq!(
        agent_field(server, "still_done", "agent_status"),
        json!("done")
    );

    // And the correct token still clears it.
    let args = acknowledge_args(&fixture.agent_pane, &token);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = server.cli(&arg_refs);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let response: Value = serde_json::from_str(&stdout).expect("acknowledge prints JSON");
    assert_eq!(response["result"]["outcome"], "acknowledged", "{response}");
}
