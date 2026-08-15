//! One upstream connection: the proxy's MCP *client* face.
//!
//! [`connect`] runs the initialize handshake against a fresh transport
//! (offering [`crate::protocol::OFFERED_VERSION`], accepting only
//! [`crate::protocol::SUPPORTED_VERSIONS`], declaring **no** client
//! capabilities — the deliberate attenuation that closes the
//! server→client request channel for T1), then spawns the connection
//! actor. The actor owns the transport and this upstream's minted id
//! space; all traffic in either direction crosses it.
//!
//! Everything the actor emits toward the session router is an [`Event`]
//! tagged with the upstream's index; everything the router asks of it
//! is a [`Command`]. Responses to forwarded client requests come back
//! with the client's original id bytes already restored — the router
//! never touches upstream ids.

use std::time::Duration;

use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::builder::{self, code};
use crate::envelope::{self, Message, RequestId, ResponseId};
use crate::idmap::{Pending, PendingMap};
use crate::protocol;
use crate::splice;
use crate::transport::{Received, SendContext, Transport, TransportError};

/// Commands queued between router and one connection actor.
const COMMAND_QUEUE: usize = 64;

/// What the router may ask of a connection.
#[derive(Debug)]
pub enum Command {
    /// Forward a validated client request; the actor mints the upstream
    /// id and splices it into the frame.
    Forward {
        /// The client's request id, as parsed.
        client_id: RequestId,
        /// The client id's exact bytes, restored on the response.
        client_id_raw: Box<str>,
        /// The full original client frame.
        frame: Vec<u8>,
    },
    /// Forward a client cancellation for an in-flight request; the
    /// actor rewrites `params.requestId` into its id space and forgets
    /// the mapping, so the late response (if any) is dropped.
    Cancel {
        /// The client id of the request being cancelled.
        client_id: RequestId,
        /// The full original cancellation notification frame.
        frame: Vec<u8>,
    },
    /// A proxy-internal request (`tools/list`); the response resolves
    /// the reply channel instead of going to the client.
    Request {
        /// The MCP method.
        method: &'static str,
        /// Serialized params, if any.
        params: Option<String>,
        /// Where the result goes.
        reply: oneshot::Sender<Result<Box<RawValue>, CallError>>,
    },
    /// Close the connection: shut the transport down cleanly.
    Shutdown,
}

/// What a connection reports to the router.
#[derive(Debug)]
pub enum Event {
    /// A response to a forwarded client request, client id restored.
    Response {
        /// Which upstream this came from.
        upstream: usize,
        /// The client request this answers (for in-flight cleanup).
        client_id: RequestId,
        /// The frame to deliver, byte-faithful except the id.
        frame: Vec<u8>,
    },
    /// A server notification that passes through to the client
    /// (`notifications/progress`).
    Notification {
        /// Which upstream this came from.
        upstream: usize,
        /// The frame, untouched.
        frame: Vec<u8>,
    },
    /// The upstream declared its tool list changed; the router re-lists.
    ListChanged {
        /// Which upstream changed.
        upstream: usize,
    },
    /// The upstream's transport ended cleanly (its process exited).
    Ended {
        /// Which upstream ended.
        upstream: usize,
    },
    /// The upstream's transport failed.
    Fatal {
        /// Which upstream failed.
        upstream: usize,
        /// What happened.
        error: TransportError,
    },
}

/// Failures of a proxy-internal request.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The upstream answered with a JSON-RPC error.
    #[error("upstream returned JSON-RPC error {code}: {message}")]
    Rpc {
        /// The JSON-RPC error code.
        code: i64,
        /// The JSON-RPC error message.
        message: String,
    },
    /// The connection ended before the response arrived.
    #[error("upstream connection closed")]
    Closed,
    /// No response within the caller's deadline.
    #[error("upstream did not answer in time")]
    Timeout,
}

/// Errors establishing a connection.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The transport failed during the handshake.
    #[error("transport failed during initialize")]
    Transport(#[from] TransportError),

    /// The transport ended before the handshake completed.
    #[error("upstream closed during initialize")]
    ClosedDuringInit,

    /// No initialize result within the deadline.
    #[error("upstream did not complete initialize in time")]
    Timeout,

    /// The initialize result did not parse.
    #[error("upstream sent an unparseable initialize result")]
    BadInitializeResult,

    /// The upstream answered initialize with a JSON-RPC error.
    #[error("upstream refused initialize with error {code}: {message}")]
    Refused {
        /// The JSON-RPC error code.
        code: i64,
        /// The JSON-RPC error message.
        message: String,
    },

    /// The upstream negotiated a protocol revision the proxy does not
    /// speak.
    #[error("upstream negotiated unsupported protocol version {got:?}")]
    UnsupportedVersion {
        /// The version the upstream chose.
        got: String,
    },
}

/// What the handshake learned about an upstream.
#[derive(Debug, Clone)]
pub struct UpstreamInfo {
    /// Operator-configured name.
    pub name: String,
    /// The protocol revision this upstream connection runs.
    pub negotiated_version: String,
    /// `serverInfo.name`, if sent.
    pub server_name: Option<String>,
    /// `serverInfo.version`, if sent.
    pub server_version: Option<String>,
    /// The server's usage instructions, if sent.
    pub instructions: Option<String>,
    /// Whether the server declared the `tools` capability at all.
    pub tools_declared: bool,
    /// Whether it declared `tools.listChanged`.
    pub tools_list_changed: bool,
}

/// The router's handle to one running connection actor.
#[derive(Debug, Clone)]
pub struct Handle {
    commands: mpsc::Sender<Command>,
}

impl Handle {
    /// Queues a command; an error means the actor is gone, which the
    /// router will learn (or already has) via a Fatal/Ended event.
    pub async fn send(&self, command: Command) -> Result<(), ()> {
        self.commands.send(command).await.map_err(|_| ())
    }

    /// Runs one proxy-internal request to completion with a deadline.
    pub async fn call(
        &self,
        method: &'static str,
        params: Option<String>,
        deadline: Duration,
    ) -> Result<Box<RawValue>, CallError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Request {
                method,
                params,
                reply,
            })
            .await
            .map_err(|_| CallError::Closed)?;
        match tokio::time::timeout(deadline, response).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(CallError::Closed),
            Err(_) => Err(CallError::Timeout),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitResultWire {
    protocol_version: String,
    #[serde(default)]
    capabilities: CapabilitiesWire,
    #[serde(default)]
    server_info: Option<PeerInfoWire>,
    #[serde(default)]
    instructions: Option<String>,
}

#[derive(Deserialize, Default)]
struct CapabilitiesWire {
    #[serde(default)]
    tools: Option<ToolsCapabilityWire>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ToolsCapabilityWire {
    #[serde(default)]
    list_changed: Option<bool>,
}

#[derive(Deserialize)]
struct PeerInfoWire {
    name: Option<String>,
    version: Option<String>,
}

#[derive(Deserialize)]
struct ErrorWire {
    code: Option<i64>,
    message: Option<String>,
}

fn rpc_error_fields(error_raw: &RawValue) -> (i64, String) {
    let parsed: ErrorWire = serde_json::from_str(error_raw.get()).unwrap_or(ErrorWire {
        code: None,
        message: None,
    });
    (
        parsed.code.unwrap_or(code::INTERNAL_ERROR),
        parsed.message.unwrap_or_else(|| "unknown error".to_owned()),
    )
}

/// Runs the initialize handshake and spawns the connection actor.
///
/// On success the returned [`Handle`] accepts commands, events start
/// flowing to `events`, and the actor's join handle lets the caller
/// wait out a clean shutdown; on failure the transport is closed before
/// returning.
pub async fn connect(
    upstream: usize,
    name: &str,
    mut transport: Transport,
    init_timeout: Duration,
    events: mpsc::Sender<Event>,
) -> Result<(Handle, tokio::task::JoinHandle<()>, UpstreamInfo), ConnectError> {
    let mut pending: PendingMap<oneshot::Sender<Result<Box<RawValue>, CallError>>> =
        PendingMap::default();

    let outcome =
        tokio::time::timeout(init_timeout, initialize(name, &mut transport, &mut pending))
            .await
            .unwrap_or(Err(ConnectError::Timeout));

    let info = match outcome {
        Ok(info) => info,
        Err(err) => {
            transport.close().await;
            return Err(err);
        }
    };

    let (commands_tx, commands_rx) = mpsc::channel(COMMAND_QUEUE);
    let actor = Actor {
        upstream,
        name: name.to_owned(),
        transport,
        pending,
        events,
    };
    let join = tokio::spawn(actor.run(commands_rx));
    Ok((
        Handle {
            commands: commands_tx,
        },
        join,
        info,
    ))
}

/// The initialize request/response exchange, on the caller's task.
async fn initialize(
    name: &str,
    transport: &mut Transport,
    pending: &mut PendingMap<oneshot::Sender<Result<Box<RawValue>, CallError>>>,
) -> Result<UpstreamInfo, ConnectError> {
    let (_reply_tx, _reply_rx) = oneshot::channel();
    let init_id = pending.insert_internal(_reply_tx);
    let params = serde_json::json!({
        "protocolVersion": protocol::OFFERED_VERSION,
        // Deliberately empty: the proxy declares no roots, sampling, or
        // elicitation upstream, closing the server→client request
        // channel (T1 plan, "Protocol").
        "capabilities": {},
        "clientInfo": {
            "name": protocol::PROXY_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
    .to_string();
    transport
        .send(
            builder::request_frame(init_id, "initialize", Some(&params)),
            SendContext::Request {
                upstream_id: init_id,
            },
        )
        .await?;

    // Read until the initialize response; anything else a server emits
    // this early is dropped with a log line (it has not been told the
    // session is initialized, so nothing it sends can be load-bearing).
    let result_raw: Box<RawValue> = loop {
        match transport.recv().await? {
            None => return Err(ConnectError::ClosedDuringInit),
            Some(Received::Dropped(reason)) => {
                warn!(upstream = name, ?reason, "dropped frame during initialize");
            }
            Some(Received::Frame(frame)) => match envelope::parse(&frame) {
                Ok(Message::Response {
                    id: ResponseId::Id(RequestId::Number(n)),
                    result,
                    error,
                }) if n >= 0 && pending.complete(n as u64).is_some() => match (result, error) {
                    (Some(result), None) => break result.to_owned(),
                    (None, Some(error)) => {
                        let (code, message) = rpc_error_fields(error);
                        return Err(ConnectError::Refused { code, message });
                    }
                    // Unreachable: the envelope boundary guarantees
                    // exactly one of result/error.
                    _ => return Err(ConnectError::BadInitializeResult),
                },
                Ok(other) => {
                    debug!(
                        upstream = name,
                        "ignoring pre-initialize {} from upstream",
                        message_kind(&other)
                    );
                }
                Err(err) => {
                    warn!(upstream = name, error = %err, "dropping unparseable frame during initialize");
                }
            },
        }
    };

    let parsed: InitResultWire =
        serde_json::from_str(result_raw.get()).map_err(|_| ConnectError::BadInitializeResult)?;
    if !protocol::supported(&parsed.protocol_version) {
        return Err(ConnectError::UnsupportedVersion {
            got: parsed.protocol_version,
        });
    }

    transport.set_protocol_version(&parsed.protocol_version);
    transport
        .send(
            builder::notification_frame("notifications/initialized", None),
            SendContext::FireAndForget,
        )
        .await?;
    transport.start_listening();

    let (server_name, server_version) = match parsed.server_info {
        Some(info) => (info.name, info.version),
        None => (None, None),
    };
    let info = UpstreamInfo {
        name: name.to_owned(),
        negotiated_version: parsed.protocol_version,
        server_name,
        server_version,
        instructions: parsed.instructions,
        tools_declared: parsed.capabilities.tools.is_some(),
        tools_list_changed: parsed
            .capabilities
            .tools
            .and_then(|t| t.list_changed)
            .unwrap_or(false),
    };
    info!(
        upstream = name,
        negotiated_protocol_version = info.negotiated_version,
        server_name = info.server_name.as_deref(),
        server_version = info.server_version.as_deref(),
        tools_declared = info.tools_declared,
        "upstream initialized"
    );
    Ok(info)
}

fn message_kind(message: &Message<'_>) -> &'static str {
    match message {
        Message::Request { .. } => "request",
        Message::Notification { .. } => "notification",
        Message::Response { .. } => "response",
    }
}

struct Actor {
    upstream: usize,
    name: String,
    transport: Transport,
    pending: PendingMap<oneshot::Sender<Result<Box<RawValue>, CallError>>>,
    events: mpsc::Sender<Event>,
}

impl Actor {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let mut end: Option<Event> = None;
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => break,
                    Some(command) => {
                        if let Err(event) = self.handle_command(command).await {
                            end = event;
                            break;
                        }
                    }
                },
                item = self.transport.recv() => match item {
                    Ok(Some(Received::Frame(frame))) => {
                        if self.handle_upstream_frame(frame).await.is_err() {
                            // Router gone; nothing left to serve.
                            break;
                        }
                    }
                    Ok(Some(Received::Dropped(reason))) => {
                        warn!(upstream = %self.name, ?reason, "dropped upstream frame");
                    }
                    Ok(None) => {
                        end = Some(Event::Ended { upstream: self.upstream });
                        break;
                    }
                    Err(error) => {
                        end = Some(Event::Fatal { upstream: self.upstream, error });
                        break;
                    }
                },
            }
        }

        // Teardown: internal callers learn the connection is gone;
        // client entries die with the session the router is ending.
        for entry in self.pending.drain() {
            if let Pending::Internal(reply) = entry {
                let _ = reply.send(Err(CallError::Closed));
            }
        }
        if let Some(event) = end {
            let _ = self.events.send(event).await;
        }
        self.transport.close().await;
    }

    /// Handles one router command. `Err` carries the loop-ending event
    /// (transport death), if any.
    async fn handle_command(&mut self, command: Command) -> Result<(), Option<Event>> {
        match command {
            Command::Forward {
                client_id,
                client_id_raw,
                frame,
            } => {
                let upstream_id = self
                    .pending
                    .insert_client(client_id.clone(), &client_id_raw);
                let rewritten = std::str::from_utf8(&frame)
                    .map_err(|_| ())
                    .and_then(|text| {
                        splice::rewrite_member(text, "id", &upstream_id.to_string()).map_err(|_| ())
                    });
                match rewritten {
                    Ok(rewritten) => {
                        self.send_upstream(
                            rewritten.into_bytes(),
                            SendContext::Request { upstream_id },
                        )
                        .await
                    }
                    Err(()) => {
                        // The frame parsed at the envelope boundary but
                        // resists unambiguous rewriting (duplicate
                        // unmodeled members). Fail closed: it never
                        // reaches the upstream.
                        self.pending.cancel_client(&client_id);
                        warn!(upstream = %self.name, "client frame resists id rewrite; rejecting");
                        self.emit(Event::Response {
                            upstream: self.upstream,
                            client_id,
                            frame: builder::error_frame(
                                &client_id_raw,
                                code::INVALID_REQUEST,
                                "Invalid Request",
                            ),
                        })
                        .await
                        .map_err(|_| None)
                    }
                }
            }
            Command::Cancel { client_id, frame } => {
                let Some(upstream_id) = self.pending.cancel_client(&client_id) else {
                    debug!(upstream = %self.name, "cancellation for a request not in flight; dropped");
                    return Ok(());
                };
                match rewrite_cancel(&frame, upstream_id) {
                    Ok(rewritten) => {
                        self.send_upstream(rewritten, SendContext::FireAndForget)
                            .await
                    }
                    Err(()) => {
                        // The mapping is already gone, which is the
                        // cancellation's real effect: the late response
                        // will be dropped either way.
                        warn!(upstream = %self.name, "cancellation frame resists rewrite; not forwarded");
                        Ok(())
                    }
                }
            }
            Command::Request {
                method,
                params,
                reply,
            } => {
                let upstream_id = self.pending.insert_internal(reply);
                let frame = builder::request_frame(upstream_id, method, params.as_deref());
                self.send_upstream(frame, SendContext::Request { upstream_id })
                    .await
            }
            Command::Shutdown => unreachable!("handled by the caller"),
        }
    }

    async fn send_upstream(
        &mut self,
        frame: Vec<u8>,
        ctx: SendContext,
    ) -> Result<(), Option<Event>> {
        match self.transport.send(frame, ctx).await {
            Ok(()) => Ok(()),
            Err(error) => {
                warn!(upstream = %self.name, error = %error, "failed to send to upstream");
                Err(Some(Event::Fatal {
                    upstream: self.upstream,
                    error,
                }))
            }
        }
    }

    /// Classifies one upstream frame. `Err` means the router is gone.
    async fn handle_upstream_frame(&mut self, frame: Vec<u8>) -> Result<(), ()> {
        let message = match envelope::parse(&frame) {
            Ok(message) => message,
            Err(err) => {
                warn!(upstream = %self.name, error = %err, "dropping unparseable upstream frame");
                return Ok(());
            }
        };
        match message {
            Message::Response {
                id: ResponseId::Id(RequestId::Number(n)),
                result,
                error,
            } if n >= 0 => {
                let (result, error) = (
                    result.map(RawValue::to_owned),
                    error.map(RawValue::to_owned),
                );
                match self.pending.complete(n as u64) {
                    Some(Pending::Client {
                        client_id,
                        client_id_raw,
                    }) => {
                        let restored =
                            std::str::from_utf8(&frame)
                                .map_err(|_| ())
                                .and_then(|text| {
                                    splice::rewrite_member(text, "id", &client_id_raw)
                                        .map_err(|_| ())
                                });
                        let frame = match restored {
                            Ok(text) => text.into_bytes(),
                            Err(()) => {
                                // The upstream's response resists
                                // unambiguous rewriting; the client
                                // still gets an answer, a typed one.
                                warn!(upstream = %self.name, "upstream response resists id rewrite; substituting an error");
                                builder::error_frame(
                                    &client_id_raw,
                                    code::INTERNAL_ERROR,
                                    "upstream response could not be translated",
                                )
                            }
                        };
                        self.emit(Event::Response {
                            upstream: self.upstream,
                            client_id,
                            frame,
                        })
                        .await
                    }
                    Some(Pending::Internal(reply)) => {
                        let outcome = match (result, error) {
                            (Some(result), None) => Ok(result),
                            (None, Some(error)) => {
                                let (code, message) = rpc_error_fields(&error);
                                Err(CallError::Rpc { code, message })
                            }
                            // Unreachable past the envelope boundary.
                            _ => Err(CallError::Closed),
                        };
                        let _ = reply.send(outcome);
                        Ok(())
                    }
                    None => {
                        debug!(upstream = %self.name, id = n, "late or unknown response; dropped");
                        Ok(())
                    }
                }
            }
            Message::Response { .. } => {
                debug!(upstream = %self.name, "response with an id the proxy never minted; dropped");
                Ok(())
            }
            Message::Notification { method, .. } => match method.as_str() {
                "notifications/tools/list_changed" => {
                    self.emit(Event::ListChanged {
                        upstream: self.upstream,
                    })
                    .await
                }
                "notifications/progress" => {
                    self.emit(Event::Notification {
                        upstream: self.upstream,
                        frame,
                    })
                    .await
                }
                other => {
                    // The proxy advertised only `tools` to the client;
                    // notifications for undeclared capabilities
                    // (logging, resources, prompts) stop here.
                    debug!(upstream = %self.name, method = other, "dropping upstream notification outside advertised capabilities");
                    Ok(())
                }
            },
            Message::Request { id, method, .. } => {
                // The proxy declared no client capabilities upstream, so
                // no server request is legitimate; `ping` is answered as
                // a liveness courtesy, everything else is refused.
                let reply = if method == "ping" {
                    builder::result_frame(&builder::encode_id(&id), "{}")
                } else {
                    debug!(upstream = %self.name, method, "refusing upstream request; server→client channel is closed");
                    builder::error_frame_for(&id, code::METHOD_NOT_FOUND, "Method not found")
                };
                // A send failure here is transport death; the recv side
                // will surface it as Fatal on the next loop turn.
                if let Err(event) = self.send_upstream(reply, SendContext::FireAndForget).await {
                    if let Some(event) = event {
                        let _ = self.events.send(event).await;
                    }
                    return Err(());
                }
                Ok(())
            }
        }
    }

    async fn emit(&mut self, event: Event) -> Result<(), ()> {
        self.events.send(event).await.map_err(|_| ())
    }
}

/// Rewrites a client cancellation into the upstream id space:
/// `params.requestId` becomes the minted id; every other byte of params
/// and frame members is preserved.
fn rewrite_cancel(frame: &[u8], upstream_id: u64) -> Result<Vec<u8>, ()> {
    let text = std::str::from_utf8(frame).map_err(|_| ())?;
    let Ok(Message::Notification {
        params: Some(params),
        ..
    }) = envelope::parse(frame)
    else {
        return Err(());
    };
    let new_params = splice::rewrite_member(params.get(), "requestId", &upstream_id.to_string())
        .map_err(|_| ())?;
    let rewritten = splice::rewrite_member(text, "params", &new_params).map_err(|_| ())?;
    Ok(rewritten.into_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cancel_rewrite_translates_only_the_request_id() {
        let frame = br#"{"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "client-9", "reason": "user changed  mind"}, "_meta": {"k": [1e2]}}"#;
        let out = rewrite_cancel(frame, 42).unwrap();
        assert_eq!(
            out,
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":42,"reason":"user changed  mind"},"_meta":{"k": [1e2]}}"#.to_vec()
        );
    }

    #[test]
    fn cancel_without_params_or_request_id_is_refused() {
        let no_params = br#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#;
        assert!(rewrite_cancel(no_params, 1).is_err());
        let no_request_id =
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"reason":"x"}}"#;
        assert!(rewrite_cancel(no_request_id, 1).is_err());
    }

    #[test]
    fn rpc_error_fields_tolerate_malformed_errors() {
        let raw: Box<RawValue> =
            serde_json::from_str(r#"{"code": -32000, "message": "boom"}"#).unwrap();
        assert_eq!(rpc_error_fields(&raw), (-32000, "boom".to_owned()));
        let raw: Box<RawValue> = serde_json::from_str(r#"{"weird": true}"#).unwrap();
        let (code, message) = rpc_error_fields(&raw);
        assert_eq!(code, code::INTERNAL_ERROR);
        assert_eq!(message, "unknown error");
    }
}
