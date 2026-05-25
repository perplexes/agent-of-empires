//! Regression coverage for the wedge documented in
//! `https://github.com/njbrake/agent-of-empires/issues/1196`:
//!
//! When the ACP adapter ignores `session/cancel`, the daemon used to
//! sit on the in-flight `prompt_fut` forever. The user's "Stop"
//! cleared the spinner via a synthetic `Stopped { user_forced }`, but
//! any follow-up prompt was dropped silently (issue #1031's mid-turn
//! gate) with the daemon still wedged. The new behavior (introduced
//! across #1196 / #1240 / #1281): a follow-up prompt arriving while
//! `cancelling=true` immediately emits `PromptRejected` AND escalates
//! to `Stopped { reason: "agent_unresponsive" }`, ending the
//! connection task so the supervisor drain path can SIGTERM the
//! runner and respawn it via `session/load`.
//!
//! The test stands up the Node test shim with an `IGNORE_CANCEL`
//! prompt mode (parks the prompt; cancel handler deliberately doesn't
//! resolve it), sends prompt → cancel → prompt through `AcpClient`,
//! and asserts both the `PromptRejected` event and the terminal
//! `Stopped { reason: "agent_unresponsive" }`.
//!
//! Skipped automatically if `node` is missing. `serve`-feature only
//! because `AcpClient` lives behind that.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_of_empires::cockpit::acp_client::AcpClient;
use agent_of_empires::cockpit::state::{CockpitSessionId, Event};
use serial_test::serial;
use tokio::net::UnixListener;
use tokio::process::Command;

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn shim_path() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    PathBuf::from(manifest)
        .join("cockpit-worker")
        .join("test-shim")
        .join("shim.mjs")
}

/// Stand up the Node shim over a UNIX socket bridge and preseed the
/// session id so `AcpClient::attach` can skip `session/new`.
async fn spawn_shim_socket_bridge_with_preseed(
    preseed_session_id: &str,
) -> (PathBuf, tempfile::TempDir) {
    let shim = shim_path();
    let temp = tempfile::tempdir().unwrap();
    let socket_path = temp.path().join("runner.sock");

    let mut cmd = Command::new("node");
    cmd.arg(&shim);
    cmd.env("SHIM_PRESEED_SESSION_ID", preseed_session_id);
    let mut shim_proc = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn shim");
    let shim_stdin = shim_proc.stdin.take().expect("shim stdin");
    let shim_stdout = shim_proc.stdout.take().expect("shim stdout");

    let listener = UnixListener::bind(&socket_path).expect("bind listener");

    tokio::spawn(async move {
        let _shim_proc = shim_proc;
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => return,
        };
        let (mut sock_read, mut sock_write) = stream.into_split();
        let mut shim_in = shim_stdin;
        let mut shim_out = shim_stdout;
        let to_shim = async move { tokio::io::copy(&mut sock_read, &mut shim_in).await.ok() };
        let from_shim = async move { tokio::io::copy(&mut shim_out, &mut sock_write).await.ok() };
        let _ = tokio::join!(to_shim, from_shim);
    });

    (socket_path, temp)
}

/// Drain events until we see the shim's first `AgentMessageChunk`,
/// proving the daemon has dispatched the prompt and the shim is now
/// parked inside its IGNORE_CANCEL branch. Returns false if the
/// deadline expires before we see it.
async fn wait_for_first_chunk(client: &mut AcpClient, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await {
            Ok(Some(Event::AgentMessageChunk { .. })) => return true,
            Ok(Some(_)) => continue,
            Ok(None) => return false,
            Err(_) => continue,
        }
    }
    false
}

#[derive(Default)]
struct DrainOutcome {
    rejected: Option<(String, String)>,
    stopped: Option<String>,
}

/// Drain events until both `PromptRejected` and a terminal `Stopped`
/// have been observed (or the deadline expires). Order is
/// PromptRejected → Stopped per `acp_client.rs:3852-3935`.
async fn drain_for_rejection_and_stopped(
    client: &mut AcpClient,
    deadline: Instant,
) -> DrainOutcome {
    let mut out = DrainOutcome::default();
    while Instant::now() < deadline {
        if out.rejected.is_some() && out.stopped.is_some() {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await {
            Ok(Some(Event::PromptRejected { reason, text })) => {
                out.rejected = Some((reason, text));
            }
            Ok(Some(Event::Stopped { reason })) => {
                out.stopped = Some(reason);
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    out
}

#[tokio::test]
#[serial]
async fn ignored_cancel_then_retry_emits_promptrejected_and_escalates() {
    if !node_available() || !shim_path().exists() {
        eprintln!("skipping: node or shim missing");
        return;
    }

    let preseed = "cancel-escalation-1";
    let (socket_path, _tmp) = spawn_shim_socket_bridge_with_preseed(preseed).await;

    let mut client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        CockpitSessionId("cancel-escalation-1".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach client");

    let retry_text = "retry while cancelling";

    client
        .send_prompt("IGNORE_CANCEL stuck")
        .await
        .expect("send initial prompt");

    assert!(
        wait_for_first_chunk(&mut client, Instant::now() + Duration::from_secs(3)).await,
        "expected shim to emit first AgentMessageChunk before cancel"
    );

    client.cancel_prompt().await.expect("send cancel");

    // Give the daemon's select loop a tick to flip `cancelling=true`
    // and arm the cancel-escalation grace before we race the retry
    // prompt against it.
    tokio::time::sleep(Duration::from_millis(50)).await;

    client
        .send_prompt(retry_text)
        .await
        .expect("send retry prompt");

    let outcome =
        drain_for_rejection_and_stopped(&mut client, Instant::now() + Duration::from_secs(3)).await;
    let _ = client.shutdown().await;

    let (reason, text) = outcome
        .rejected
        .as_ref()
        .expect("expected a PromptRejected event for the retry prompt");
    assert_eq!(reason, "agent_busy", "PromptRejected reason mismatch");
    assert_eq!(
        text, retry_text,
        "PromptRejected must echo the retry prompt text so the UI can re-fire it"
    );

    assert_eq!(
        outcome.stopped.as_deref(),
        Some("agent_unresponsive"),
        "expected the connection task to escalate the wedged adapter to \
         Stopped {{ reason: agent_unresponsive }} when a retry prompt \
         lands while cancel is pending; observed {:?}",
        outcome.stopped
    );
}
