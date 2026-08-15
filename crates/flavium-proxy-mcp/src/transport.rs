//! Upstream transports: one uniform frame-in/frame-out surface over
//! stdio child processes and streamable HTTP endpoints.
//!
//! A transport moves opaque frames; it never interprets JSON-RPC beyond
//! what its wire format forces on it (the HTTP transport must know
//! whether a frame expects a response — see [`SendContext`]). Protocol
//! behavior lives in the connection actor above this seam.
//!
//! Failure taxonomy, uniform across transports:
//! - **Dropped items** ([`Received::Dropped`]) are per-frame problems
//!   (oversized, wrong SSE event type); the transport stays usable.
//! - **Fatal errors** ([`TransportError`]) end the upstream connection:
//!   a dead pipe, an expired HTTP session.
//! - HTTP-only: a failed POST for a single request is neither — the
//!   transport synthesizes a JSON-RPC error *response* for that request
//!   so the caller's pending-map accounting resolves normally.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info, warn};

use crate::framing::{write_frame, FrameReadError, FrameReader};
use crate::http::HttpTransport;

/// How long a spawned upstream child gets to exit after its stdin
/// closes before it is killed — the MCP stdio shutdown sequence.
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(5);

/// What the caller knows about a frame it is sending; the HTTP
/// transport needs this to run the streamable-HTTP state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendContext {
    /// A request in the proxy's minted id space; a response with this
    /// id is expected back.
    Request {
        /// The minted upstream-side id.
        upstream_id: u64,
    },
    /// A notification or a response: nothing comes back for it.
    FireAndForget,
}

/// A non-fatal, per-frame drop at the transport boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// The frame exceeded the size cap.
    Oversized {
        /// The configured cap.
        limit: usize,
    },
    /// An SSE event arrived with a non-default event type the proxy
    /// does not speak (e.g. the deprecated HTTP+SSE `endpoint` event).
    UnexpectedSseEvent {
        /// The event type as sent.
        event_type: String,
    },
}

/// One received item.
#[derive(Debug)]
pub enum Received {
    /// A frame to hand to the protocol layer.
    Frame(Vec<u8>),
    /// A frame was discarded; accounting only.
    Dropped(DropReason),
}

/// A connection-fatal transport failure.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The underlying byte stream failed.
    #[error("transport i/o error")]
    Io(#[from] std::io::Error),

    /// A frame to be written contained an embedded newline; refusing it
    /// is fail-closed framing discipline (should be unreachable: every
    /// outbound frame is either framed input or proxy-built).
    #[error("outbound frame contains an embedded newline")]
    EmbeddedNewline,

    /// The HTTP server declared the MCP session gone (404 with a
    /// session id). T1/M2 treats this as fatal rather than silently
    /// re-initializing into a state the client never observed.
    #[error("upstream ended the MCP session")]
    SessionExpired,
}

/// Errors spawning a stdio upstream.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The command line was empty.
    #[error("upstream command is empty")]
    EmptyCommand,

    /// The process could not be started.
    #[error("failed to spawn upstream `{command}`")]
    Spawn {
        /// The program that failed to start.
        command: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The spawned child was missing a piped stream — an internal bug.
    #[error("upstream child had no piped {stream}")]
    MissingPipe {
        /// Which stream was missing.
        stream: &'static str,
    },
}

/// Boxed byte streams so one stdio transport type serves both child
/// process pipes (production) and in-memory duplexes (tests).
type BoxedRead = Box<dyn AsyncRead + Send + Unpin>;
type BoxedWrite = Box<dyn AsyncWrite + Send + Unpin>;

/// An upstream reached over newline-delimited frames on a byte stream
/// pair — a spawned child's stdin/stdout in production.
pub struct StdioTransport {
    reader: FrameReader<BoxedRead>,
    /// `None` once closed; dropping the writer closes the child's
    /// stdin, which is the MCP stdio shutdown signal.
    writer: Option<BoxedWrite>,
    child: Option<tokio::process::Child>,
}

impl StdioTransport {
    /// Spawns `command` as a child process, MCP on its stdin/stdout,
    /// stderr inherited so its logs surface beside the proxy's.
    pub fn spawn(command: &[String], max_frame: usize) -> Result<Self, SpawnError> {
        let (program, args) = command.split_first().ok_or(SpawnError::EmptyCommand)?;
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| SpawnError::Spawn {
                command: program.clone(),
                source,
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or(SpawnError::MissingPipe { stream: "stdin" })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(SpawnError::MissingPipe { stream: "stdout" })?;
        Ok(Self {
            reader: FrameReader::new(Box::new(stdout), max_frame),
            writer: Some(Box::new(stdin)),
            child: Some(child),
        })
    }

    /// A transport over caller-provided streams; how tests stand in for
    /// a child process.
    pub fn from_streams<R, W>(reader: R, writer: W, max_frame: usize) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            reader: FrameReader::new(Box::new(reader), max_frame),
            writer: Some(Box::new(writer)),
            child: None,
        }
    }

    async fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let writer = self.writer.as_mut().ok_or_else(closed_pipe)?;
        write_frame(writer, frame).await.map_err(|err| match err {
            crate::framing::FrameWriteError::EmbeddedNewline => TransportError::EmbeddedNewline,
            crate::framing::FrameWriteError::Io(io) => TransportError::Io(io),
        })
    }

    async fn recv(&mut self) -> Result<Option<Received>, TransportError> {
        match self.reader.read_frame().await {
            Ok(Some(frame)) => Ok(Some(Received::Frame(frame))),
            Ok(None) => Ok(None),
            Err(FrameReadError::Oversized { limit }) => {
                Ok(Some(Received::Dropped(DropReason::Oversized { limit })))
            }
            Err(FrameReadError::Io(err)) => Err(TransportError::Io(err)),
        }
    }

    /// Closes the child's stdin and reaps it, killing it if it ignores
    /// the shutdown signal past the grace period.
    async fn close(&mut self) {
        drop(self.writer.take());
        if let Some(mut child) = self.child.take() {
            match tokio::time::timeout(CHILD_EXIT_GRACE, child.wait()).await {
                Ok(Ok(status)) => info!(%status, "upstream exited"),
                Ok(Err(err)) => warn!(error = %err, "failed to wait for upstream"),
                Err(_) => {
                    warn!("upstream did not exit after its stdin closed; killing it");
                    if let Err(err) = child.kill().await {
                        warn!(error = %err, "failed to kill upstream");
                    }
                }
            }
        }
    }
}

fn closed_pipe() -> TransportError {
    TransportError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "transport already closed",
    ))
}

/// The transport for one upstream connection.
pub enum Transport {
    /// Newline-delimited frames over a process's stdio. Boxed: the
    /// frame reader's buffer makes this variant much larger than the
    /// HTTP one.
    Stdio(Box<StdioTransport>),
    /// Streamable HTTP against an MCP endpoint.
    Http(HttpTransport),
}

impl Transport {
    /// Wraps a stdio transport.
    pub fn stdio(transport: StdioTransport) -> Self {
        Self::Stdio(Box::new(transport))
    }

    /// Wraps an HTTP transport.
    pub fn http(transport: HttpTransport) -> Self {
        Self::Http(transport)
    }

    /// Sends one frame. Fatal errors only; per-request HTTP failures
    /// surface as synthesized error responses via [`Transport::recv`].
    pub async fn send(&mut self, frame: Vec<u8>, ctx: SendContext) -> Result<(), TransportError> {
        match self {
            Self::Stdio(t) => t.send(&frame).await,
            Self::Http(t) => t.send(frame, ctx).await,
        }
    }

    /// Receives the next item. `Ok(None)` is a clean end of stream.
    pub async fn recv(&mut self) -> Result<Option<Received>, TransportError> {
        match self {
            Self::Stdio(t) => t.recv().await,
            Self::Http(t) => t.recv().await,
        }
    }

    /// Records the negotiated protocol version; the HTTP transport
    /// advertises it on all subsequent requests (`MCP-Protocol-Version`).
    pub fn set_protocol_version(&mut self, version: &str) {
        if let Self::Http(t) = self {
            t.set_protocol_version(version);
        }
    }

    /// Opens the server→client listening channel where the transport
    /// has one (the HTTP GET stream); a no-op for stdio.
    pub fn start_listening(&mut self) {
        if let Self::Http(t) = self {
            t.start_listening();
        }
    }

    /// Shuts the transport down: closes and reaps a stdio child,
    /// terminates an HTTP session (DELETE) and its tasks.
    pub async fn close(&mut self) {
        match self {
            Self::Stdio(t) => t.close().await,
            Self::Http(t) => t.close().await,
        }
    }
}

/// Replaces raw newline bytes with spaces so a frame received from a
/// newline-agnostic wire (HTTP bodies, SSE data) can traverse the
/// newline-delimited stdio framing.
///
/// In well-formed JSON a raw `\n`/`\r` byte can only occur *between*
/// tokens (inside strings they must be escaped), so this substitution
/// preserves every token byte exactly; a frame that violates that was
/// never valid JSON and will be rejected at the envelope boundary.
pub fn normalize_newlines(mut frame: Vec<u8>) -> Vec<u8> {
    for byte in &mut frame {
        if *byte == b'\n' || *byte == b'\r' {
            *byte = b' ';
        }
    }
    frame
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn stdio_transport_round_trips_frames() {
        let (test_side_tx, transport_rx) = tokio::io::duplex(4096);
        let (transport_tx, mut test_side_rx) = tokio::io::duplex(4096);
        let mut t = Transport::stdio(StdioTransport::from_streams(
            transport_rx,
            transport_tx,
            1024,
        ));

        t.send(
            b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"ping\"}".to_vec(),
            SendContext::Request { upstream_id: 0 },
        )
        .await
        .unwrap();
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 256];
        let n = test_side_rx.read(&mut buf).await.unwrap();
        assert_eq!(
            &buf[..n],
            b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"ping\"}\n"
        );

        let mut writer = test_side_tx;
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}\n")
            .await
            .unwrap();
        match t.recv().await.unwrap().unwrap() {
            Received::Frame(frame) => {
                assert_eq!(frame, b"{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}");
            }
            other => panic!("expected frame, got {other:?}"),
        }

        // Closing the peer produces a clean end of stream.
        drop(writer);
        assert!(matches!(t.recv().await, Ok(None)));
    }

    #[tokio::test]
    async fn oversized_stdio_frame_is_dropped_not_fatal() {
        let (test_side_tx, transport_rx) = tokio::io::duplex(65536);
        let (transport_tx, _keep) = tokio::io::duplex(4096);
        let mut t = Transport::stdio(StdioTransport::from_streams(transport_rx, transport_tx, 64));

        let mut writer = test_side_tx;
        let mut big = vec![b'x'; 256];
        big.push(b'\n');
        writer.write_all(&big).await.unwrap();
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"n\"}\n")
            .await
            .unwrap();

        assert!(matches!(
            t.recv().await.unwrap().unwrap(),
            Received::Dropped(DropReason::Oversized { limit: 64 })
        ));
        assert!(matches!(
            t.recv().await.unwrap().unwrap(),
            Received::Frame(_)
        ));
    }

    #[test]
    fn normalize_newlines_touches_only_newline_bytes() {
        let input = b"{\"a\":\r\n  [1,\n 2.5],\r\"s\":\"esc\\naped\"}".to_vec();
        let out = normalize_newlines(input);
        assert_eq!(out, b"{\"a\":    [1,  2.5], \"s\":\"esc\\naped\"}".to_vec());
        // Still-valid JSON with identical token bytes; the escaped \n
        // inside the string survives untouched.
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["s"], "esc\naped");
    }
}
