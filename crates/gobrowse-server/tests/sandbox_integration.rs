//! Real-daemon integration for the sandbox client.
//!
//! Gated on `GOBROWSE_SANDBOX_SOCKET_PATH` (the Unix socket of a running
//! gobrowse-sandboxd); the test skips when it is unset. The auth token is read
//! from `GOBROWSE_SANDBOX_AUTH_TOKEN` or, failing that, from `auth.token` next
//! to the socket.
//!
//! The daemon must run with `--quota-managed-workspaces`; its Start operation
//! verifies that the workspace's podman named volume exists and matches the
//! provisioned workspace root, so the test creates the volume with podman
//! (using the ambient `CONTAINERS_CONF`, which must point the volume path at
//! the daemon's workspace root) before the terminal round trip.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use gobrowse_core::sandbox::{
    HARD_RESOURCE_LIMITS, NetworkPolicy, TerminalStartRequest, workspace_volume_name,
};
use gobrowse_server::sandbox_client::{SandboxClient, SandboxConfig};
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const PODMAN: &str = "/usr/bin/podman";

fn socket_path_from_env() -> Option<PathBuf> {
    std::env::var_os("GOBROWSE_SANDBOX_SOCKET_PATH").map(PathBuf::from)
}

fn auth_token(socket_path: &Path) -> SecretString {
    if let Ok(token) = std::env::var("GOBROWSE_SANDBOX_AUTH_TOKEN") {
        return SecretString::from(token);
    }
    let token_file = socket_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("auth.token");
    let token = std::fs::read_to_string(&token_file)
        .expect("auth.token must sit next to the sandbox socket");
    SecretString::from(token.trim().to_owned())
}

async fn connect() -> SandboxClient {
    let socket_path = socket_path_from_env().expect("GOBROWSE_SANDBOX_SOCKET_PATH must be set");
    let token = auth_token(&socket_path);
    SandboxClient::connect(SandboxConfig {
        socket_path,
        auth_token: token,
        timeout: Duration::from_secs(30),
    })
    .await
    .expect("sandbox client configuration is valid")
}

#[tokio::test]
async fn real_daemon_health_provision_fs_and_terminal_round_trip() {
    if socket_path_from_env().is_none() {
        eprintln!("skipping sandbox integration: GOBROWSE_SANDBOX_SOCKET_PATH is unset");
        return;
    }
    let client = connect().await;

    // 1. Health.
    assert_eq!(client.health().await.expect("health succeeds"), "ok");

    // 2. ProvisionWorkspace is idempotent.
    let workspace_id = Uuid::now_v7();
    client
        .provision_workspace(workspace_id)
        .await
        .expect("first provision succeeds");
    client
        .provision_workspace(workspace_id)
        .await
        .expect("second provision succeeds (idempotent)");

    // 3. Filesystem round trip.
    client
        .fs_mkdir(workspace_id, "sub")
        .await
        .expect("mkdir succeeds");
    let payload = b"hello sandbox";
    let (bytes, digest) = client
        .fs_write(workspace_id, "sub/hello.txt", payload)
        .await
        .expect("write succeeds");
    assert_eq!(bytes, payload.len());
    assert_eq!(digest, hex(&Sha256::digest(payload)));
    let entries = client
        .fs_list(workspace_id, "sub")
        .await
        .expect("list succeeds");
    let hello = entries
        .iter()
        .find(|entry| entry.name == "hello.txt")
        .expect("hello.txt is listed");
    assert_eq!(hello.size, payload.len() as u64);
    let (read_back, read_digest) = client
        .fs_read(workspace_id, "sub/hello.txt")
        .await
        .expect("read succeeds");
    assert_eq!(read_back, payload);
    assert_eq!(read_digest, digest);

    // 4. Terminal round trip (needs the quota-managed podman volume to exist).
    let volume_name = workspace_volume_name(workspace_id);
    let status = std::process::Command::new(PODMAN)
        .args(["volume", "create", "--ignore", &volume_name])
        .status()
        .expect("podman must be available for the sandbox terminal test");
    assert!(
        status.success(),
        "podman volume create failed for {volume_name}"
    );

    let terminal_id = Uuid::now_v7();
    let started = client
        .terminal_start(
            terminal_id,
            TerminalStartRequest {
                workspace_id,
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf terminal-ok; sleep 30".into(),
                ],
                working_directory: ".".into(),
                cols: 80,
                rows: 24,
                network_policy: NetworkPolicy::None,
                limits: HARD_RESOURCE_LIMITS,
            },
        )
        .await
        .expect("terminal start succeeds");
    assert_eq!(started.terminal_id, terminal_id);

    let mut cursor = 0u64;
    let mut output = Vec::new();
    let mut complete = false;
    for _ in 0..40 {
        let chunk = client
            .terminal_read_output(terminal_id, cursor, 4096, 250)
            .await
            .expect("read_output succeeds");
        cursor = chunk.next_cursor;
        output.extend_from_slice(&chunk.data);
        if chunk.output_complete {
            complete = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(complete, "terminal output never reached completion");
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("terminal-ok"),
        "terminal output is missing the command echo: {text:?}"
    );

    client
        .terminal_terminate(terminal_id)
        .await
        .expect("terminate succeeds");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
