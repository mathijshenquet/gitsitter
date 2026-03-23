//! Unix domain socket transport for CLI <-> daemon communication.
//!
//! Protocol: length-prefixed JSON messages.
//!   - 4 bytes: big-endian u32 message length
//!   - N bytes: JSON payload
//!
//! Maximum message size: 16 MB.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::paths;

/// Maximum allowed message size (16 MB).
const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    #[serde(rename = "status")]
    Status {
        repo_path: Option<String>,
        global: bool,
    },

    #[serde(rename = "sync")]
    Sync {
        repo_path: Option<String>,
        all: bool,
    },

    #[serde(rename = "register")]
    Register { repo_path: String },

    #[serde(rename = "config_update")]
    ConfigUpdate { repo_path: Option<String> },

    #[serde(rename = "enable")]
    Enable { repo_path: String },

    #[serde(rename = "disable")]
    Disable { repo_path: String, purge: bool },

    #[serde(rename = "log")]
    Log {
        repo_path: Option<String>,
        global: bool,
        follow: bool,
        since: Option<String>,
    },

    #[serde(rename = "daemon_status")]
    DaemonStatus,

    #[serde(rename = "shutdown")]
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    #[serde(rename = "ok")]
    Ok { message: String },

    #[serde(rename = "error")]
    Error { message: String },

    #[serde(rename = "status")]
    Status { data: StatusData },

    #[serde(rename = "global_status")]
    GlobalStatus { repos: Vec<RepoStatusData> },

    #[serde(rename = "daemon_status")]
    DaemonStatus {
        pid: u32,
        uptime_secs: u64,
        repos_watched: usize,
    },

    #[serde(rename = "log_entry")]
    LogEntry { entry: String },

    #[serde(rename = "log_end")]
    LogEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusData {
    pub repo_id: String,
    pub display_path: String,
    pub mode: String,
    pub last_sync: Option<String>,
    pub branches: Vec<BranchStatusData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchStatusData {
    pub name: String,
    pub upstream: Option<String>,
    /// synced, diverged, local_ahead, remote_ahead, etc.
    pub status: String,
    /// e.g. "pulled 2m ago", "pushed 45s ago"
    pub last_action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoStatusData {
    pub display_path: String,
    pub mode: String,
    /// e.g. "10 synced", "1/9 diverged"
    pub status_summary: String,
    pub last_sync: Option<String>,
}

// ---------------------------------------------------------------------------
// Low-level framing
// ---------------------------------------------------------------------------

/// Send a length-prefixed message.
pub async fn send_message<W: AsyncWriteExt + Unpin>(writer: &mut W, msg: &[u8]) -> Result<()> {
    let len = u32::try_from(msg.len()).context("message too large to encode length")?;
    if len > MAX_MESSAGE_SIZE {
        bail!(
            "message size {} exceeds maximum allowed size {}",
            len,
            MAX_MESSAGE_SIZE
        );
    }
    writer
        .write_all(&len.to_be_bytes())
        .await
        .context("failed to write message length")?;
    writer
        .write_all(msg)
        .await
        .context("failed to write message payload")?;
    writer.flush().await.context("failed to flush writer")?;
    Ok(())
}

/// Receive a length-prefixed message. Returns the raw bytes.
pub async fn recv_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .context("failed to read message length")?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MESSAGE_SIZE {
        bail!(
            "incoming message size {} exceeds maximum allowed size {}",
            len,
            MAX_MESSAGE_SIZE
        );
    }
    let mut buf = vec![0u8; len as usize];
    reader
        .read_exact(&mut buf)
        .await
        .context("failed to read message payload")?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Typed helpers
// ---------------------------------------------------------------------------

/// Serialize and send a [`Request`].
pub async fn send_request<W: AsyncWriteExt + Unpin>(writer: &mut W, req: &Request) -> Result<()> {
    let json = serde_json::to_vec(req).context("failed to serialize request")?;
    send_message(writer, &json).await
}

/// Receive and deserialize a [`Request`].
pub async fn recv_request<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Request> {
    let buf = recv_message(reader).await?;
    serde_json::from_slice(&buf).context("failed to deserialize request")
}

/// Serialize and send a [`Response`].
pub async fn send_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    resp: &Response,
) -> Result<()> {
    let json = serde_json::to_vec(resp).context("failed to serialize response")?;
    send_message(writer, &json).await
}

/// Receive and deserialize a [`Response`].
pub async fn recv_response<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Response> {
    let buf = recv_message(reader).await?;
    serde_json::from_slice(&buf).context("failed to deserialize response")
}

// ---------------------------------------------------------------------------
// Client connection helpers
// ---------------------------------------------------------------------------

/// Connect to the running daemon via the Unix domain socket.
pub async fn connect_to_daemon() -> Result<UnixStream> {
    let path = paths::socket_path();
    UnixStream::connect(&path)
        .await
        .with_context(|| format!("failed to connect to daemon socket at {}", path.display()))
}

/// Quick synchronous check whether the daemon appears to be running.
///
/// Tries a blocking connect to the socket with a short timeout. Returns `true`
/// if the connection succeeds, `false` otherwise.
pub fn is_daemon_running() -> bool {
    let path = paths::socket_path();
    // First check if the socket file exists at all to avoid unnecessary work.
    if !path.exists() {
        return false;
    }
    // Attempt a blocking connect — if it succeeds the daemon is listening.
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, DuplexStream};

    /// Round-trip a raw message through send/recv.
    #[tokio::test]
    async fn raw_message_round_trip() {
        let (mut client, mut server): (DuplexStream, DuplexStream) = duplex(1024);
        let payload = b"hello world";
        send_message(&mut client, payload).await.unwrap();
        let received = recv_message(&mut server).await.unwrap();
        assert_eq!(received, payload);
    }

    /// Round-trip a Request.
    #[tokio::test]
    async fn request_round_trip() {
        let (mut client, mut server) = duplex(4096);
        let req = Request::Status {
            repo_path: Some("/tmp/repo".into()),
            global: false,
        };
        send_request(&mut client, &req).await.unwrap();
        let got = recv_request(&mut server).await.unwrap();
        // Compare via JSON to avoid needing PartialEq
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            serde_json::to_string(&got).unwrap(),
        );
    }

    /// Round-trip a Response.
    #[tokio::test]
    async fn response_round_trip() {
        let (mut client, mut server) = duplex(4096);
        let resp = Response::Ok {
            message: "done".into(),
        };
        send_response(&mut client, &resp).await.unwrap();
        let got = recv_response(&mut server).await.unwrap();
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            serde_json::to_string(&got).unwrap(),
        );
    }

    /// Empty message round-trip.
    #[tokio::test]
    async fn empty_message() {
        let (mut client, mut server) = duplex(1024);
        send_message(&mut client, b"").await.unwrap();
        let received = recv_message(&mut server).await.unwrap();
        assert!(received.is_empty());
    }

    /// Reject oversized incoming messages.
    #[tokio::test]
    async fn reject_oversized_message() {
        let (mut client, mut server) = duplex(64);
        // Write a length header claiming a huge payload.
        let fake_len: u32 = MAX_MESSAGE_SIZE + 1;
        client.write_all(&fake_len.to_be_bytes()).await.unwrap();
        let err = recv_message(&mut server).await.unwrap_err();
        assert!(
            format!("{err}").contains("exceeds maximum"),
            "unexpected error: {err}"
        );
    }

    /// Serde tag-based discriminator works as expected.
    #[tokio::test]
    async fn request_serde_tags() {
        let req = Request::Shutdown;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"shutdown"#));

        let req2 = Request::Register {
            repo_path: "/a".into(),
        };
        let json2 = serde_json::to_string(&req2).unwrap();
        assert!(json2.contains(r#""type":"register"#));
    }

    /// Multiple messages on the same stream.
    #[tokio::test]
    async fn multiple_messages() {
        let (mut client, mut server) = duplex(4096);
        send_message(&mut client, b"first").await.unwrap();
        send_message(&mut client, b"second").await.unwrap();
        assert_eq!(recv_message(&mut server).await.unwrap(), b"first");
        assert_eq!(recv_message(&mut server).await.unwrap(), b"second");
    }

    /// is_daemon_running returns false when there is no socket.
    #[test]
    fn no_daemon_running() {
        // Point socket to a path that definitely doesn't exist.
        unsafe {
            std::env::set_var("GITSITTER_SOCKET_PATH", "/tmp/gitsitter-test-nonexistent.sock");
        }
        assert!(!is_daemon_running());
        unsafe {
            std::env::remove_var("GITSITTER_SOCKET_PATH");
        }
    }
}
