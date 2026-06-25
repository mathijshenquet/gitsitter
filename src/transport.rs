//! Local transport for CLI <-> daemon communication.
//!
//! Protocol: length-prefixed JSON messages.
//!   - 4 bytes: big-endian u32 message length
//!   - N bytes: JSON payload
//!
//! Maximum message size: 16 MB.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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

    #[serde(rename = "reload_config")]
    ReloadConfig,

    #[serde(rename = "daemon_status")]
    DaemonStatus,

    #[serde(rename = "prompt_check")]
    PromptCheck { repo_path: String },

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

    #[serde(rename = "sync_complete")]
    SyncComplete {
        data: StatusData,
        events: Vec<SyncEvent>,
    },

    #[serde(rename = "global_status")]
    GlobalStatus { repos: Vec<RepoStatusData> },

    #[serde(rename = "daemon_status")]
    DaemonStatus {
        pid: u32,
        uptime_secs: u64,
        repos_watched: usize,
        /// Set when a newer version is available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latest_version: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusData {
    pub repo_id: String,
    pub display_path: String,
    pub last_sync: Option<String>,
    pub branches: Vec<BranchStatusData>,
    /// Remote names that are not trusted (host not in trusted_hosts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untrusted_remotes: Vec<String>,
    /// Unique hosts of untrusted remotes (for actionable hints).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untrusted_hosts: Vec<String>,
    /// Remote names explicitly disabled in config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_remotes: Vec<String>,
    /// All remotes: name -> URL.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub remote_urls: HashMap<String, String>,
    /// True when the repo was just auto-registered by a PromptCheck.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub newly_registered: bool,
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
    /// e.g. "10 synced", "1/9 diverged"
    pub status_summary: String,
    pub last_sync: Option<String>,
}

/// Structured event emitted during a sync cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum SyncEvent {
    #[serde(rename = "fetch")]
    Fetch { remotes: Vec<String> },

    #[serde(rename = "branch")]
    Branch {
        branch: String,
        /// MergeAnalysis result: "UpToDate", "FastForward", "LocalAhead", "Diverged", "UpstreamGone"
        analysis: String,
        /// SyncAction chosen: "FastForwardMerge", "Push", "Diverged", "DivergedNotOwned", etc.
        sync_action: String,
        /// HistoryRewrite result (only for Diverged+owned): "None", "RemoteUnchanged", "RemoteAdvanced"
        #[serde(skip_serializing_if = "Option::is_none")]
        rewrite: Option<String>,
        /// Resulting sync status: "synced", "diverged", "history_rewritten_remote_unchanged", etc.
        status: String,
        /// Human-readable description: "fast-forwarded", "rebased onto origin/main, pushed", etc.
        detail: String,
    },
}

// ---------------------------------------------------------------------------
// Low-level framing
// ---------------------------------------------------------------------------

/// Send a length-prefixed message.
pub async fn send_message<W: AsyncWrite + Unpin>(writer: &mut W, msg: &[u8]) -> Result<()> {
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
pub async fn recv_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
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
pub async fn send_request<W: AsyncWrite + Unpin>(writer: &mut W, req: &Request) -> Result<()> {
    let json = serde_json::to_vec(req).context("failed to serialize request")?;
    send_message(writer, &json).await
}

/// Receive and deserialize a [`Request`].
pub async fn recv_request<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Request> {
    let buf = recv_message(reader).await?;
    serde_json::from_slice(&buf).context("failed to deserialize request")
}

/// Serialize and send a [`Response`].
pub async fn send_response<W: AsyncWrite + Unpin>(writer: &mut W, resp: &Response) -> Result<()> {
    let json = serde_json::to_vec(resp).context("failed to serialize response")?;
    send_message(writer, &json).await
}

/// Receive and deserialize a [`Response`].
pub async fn recv_response<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Response> {
    let buf = recv_message(reader).await?;
    serde_json::from_slice(&buf).context("failed to deserialize response")
}

// ---------------------------------------------------------------------------
// Platform transport
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod platform {
    use std::path::Path;

    use anyhow::{Context, Result};
    use tokio::net::{UnixListener, UnixStream};

    pub type DaemonStream = UnixStream;
    pub type DaemonListener = UnixListener;

    pub fn cleanup_endpoint(path: &Path) {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn bind_listener(path: &Path) -> Result<DaemonListener> {
        UnixListener::bind(path)
            .with_context(|| format!("failed to bind socket at {}", path.display()))
    }

    pub async fn connect_to_daemon(path: &Path) -> Result<DaemonStream> {
        UnixStream::connect(path)
            .await
            .with_context(|| format!("failed to connect to daemon socket at {}", path.display()))
    }

    pub async fn accept(listener: &mut DaemonListener) -> Result<DaemonStream> {
        listener
            .accept()
            .await
            .map(|(stream, _)| stream)
            .context("failed to accept daemon connection")
    }

    pub fn is_daemon_running(path: &Path) -> bool {
        if !path.exists() {
            return false;
        }
        std::os::unix::net::UnixStream::connect(path).is_ok()
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use tokio::time::sleep;

    const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(25);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

    pub enum DaemonStream {
        Client(NamedPipeClient),
        Server(NamedPipeServer),
    }

    pub type DaemonListener = NamedPipeServer;

    impl AsyncRead for DaemonStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Client(stream) => Pin::new(stream).poll_read(cx, buf),
                Self::Server(stream) => Pin::new(stream).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for DaemonStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                Self::Client(stream) => Pin::new(stream).poll_write(cx, buf),
                Self::Server(stream) => Pin::new(stream).poll_write(cx, buf),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Client(stream) => Pin::new(stream).poll_flush(cx),
                Self::Server(stream) => Pin::new(stream).poll_flush(cx),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Client(stream) => Pin::new(stream).poll_shutdown(cx),
                Self::Server(stream) => Pin::new(stream).poll_shutdown(cx),
            }
        }
    }

    pub fn cleanup_endpoint(_path: &Path) {}

    fn create_server(path: &Path, first_instance: bool) -> Result<DaemonListener> {
        let mut options = ServerOptions::new();
        if first_instance {
            options.first_pipe_instance(true);
        }
        options
            .create(path)
            .with_context(|| format!("failed to create named pipe at {}", path.display()))
    }

    pub fn bind_listener(path: &Path) -> Result<DaemonListener> {
        create_server(path, true)
    }

    pub async fn connect_to_daemon(path: &Path) -> Result<DaemonStream> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            match ClientOptions::new().open(path) {
                Ok(stream) => return Ok(DaemonStream::Client(stream)),
                Err(err) if err.raw_os_error() == Some(2) => {
                    return Err(err).with_context(|| {
                        format!("failed to connect to daemon pipe at {}", path.display())
                    });
                }
                Err(err) if err.raw_os_error() == Some(231) => {
                    if Instant::now() >= deadline {
                        return Err(err).with_context(|| {
                            format!(
                                "timed out waiting for busy daemon pipe at {}",
                                path.display()
                            )
                        });
                    }
                    sleep(CONNECT_RETRY_INTERVAL).await;
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("failed to connect to daemon pipe at {}", path.display())
                    });
                }
            }
        }
    }

    pub async fn accept(listener: &mut DaemonListener, path: &Path) -> Result<DaemonStream> {
        listener
            .connect()
            .await
            .context("failed to accept daemon connection")?;
        let next = create_server(path, false)?;
        let connected = std::mem::replace(listener, next);
        Ok(DaemonStream::Server(connected))
    }

    pub fn is_daemon_running(path: &Path) -> bool {
        match ClientOptions::new().open(path) {
            Ok(_) => true,
            Err(err) if err.raw_os_error() == Some(231) => true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => false,
            Err(_) => false,
        }
    }
}

pub use platform::DaemonListener;
pub use platform::DaemonStream;

pub fn cleanup_endpoint(socket_path: &std::path::Path) {
    platform::cleanup_endpoint(socket_path);
}

pub fn bind_listener(socket_path: &std::path::Path) -> Result<platform::DaemonListener> {
    platform::bind_listener(socket_path)
}

pub async fn connect_to_daemon(socket_path: &std::path::Path) -> Result<DaemonStream> {
    platform::connect_to_daemon(socket_path).await
}

pub async fn accept_connection(
    listener: &mut platform::DaemonListener,
    #[allow(unused)] socket_path: &std::path::Path,
) -> Result<DaemonStream> {
    #[cfg(unix)]
    {
        platform::accept(listener).await
    }
    #[cfg(windows)]
    {
        platform::accept(listener, socket_path).await
    }
}

/// Quick synchronous check whether the daemon appears to be running.
pub fn is_daemon_running(socket_path: &std::path::Path) -> bool {
    platform::is_daemon_running(socket_path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{DuplexStream, duplex};

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

        let req2 = Request::ReloadConfig;
        let json2 = serde_json::to_string(&req2).unwrap();
        assert!(json2.contains(r#""type":"reload_config"#));
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

    /// is_daemon_running returns false when there is no endpoint.
    #[test]
    fn no_daemon_running() {
        let missing_path = if cfg!(windows) {
            std::path::PathBuf::from(r"\\.\pipe\gitsitter-test-nonexistent")
        } else {
            std::path::PathBuf::from("/tmp/gitsitter-test-nonexistent.sock")
        };
        assert!(!is_daemon_running(&missing_path));
    }
}
