//! The id-translation module: every mapping between the client's
//! JSON-RPC id space and the proxy's per-upstream id spaces lives here.
//!
//! The proxy speaks to each upstream in an id space it mints itself
//! (monotonic integers), for two reasons: two clients' ids must never
//! collide with the proxy's own internal requests (`initialize`,
//! `tools/list`), and a client id must never leak upstream where a
//! malicious server could forge responses to requests it was never
//! sent. Translation happens exactly twice per call: once outbound
//! (client id → minted upstream id) and once inbound (upstream id →
//! original client id bytes).
//!
//! Cancellation follows the T1 plan: forwarding a cancel *removes* the
//! mapping, so a late response from the upstream no longer translates
//! and is dropped — "late responses after cancel dropped" falls out of
//! the map itself.
//!
//! [`PendingMap`] is owned by one upstream connection; [`ClientTable`]
//! is owned by the session router and spans upstreams.

use std::collections::HashMap;

use flavium_core::CallId;

use crate::envelope::RequestId;

/// What the proxy is waiting on for one minted upstream id.
#[derive(Debug)]
pub enum Pending<T> {
    /// A forwarded client request; the response is returned to the
    /// client under its original id bytes.
    Client {
        /// The client's request id, as parsed.
        client_id: RequestId,
        /// The exact bytes of the client's id, for byte-identical
        /// round-tripping in the response.
        client_id_raw: Box<str>,
        /// Which *call* this is — the router's correlation id, echoed
        /// back so a response can be matched to the call it answers and
        /// not merely to the id slot that call once occupied.
        call_id: CallId,
    },
    /// A request the proxy originated for its own protocol needs; the
    /// response is consumed internally.
    Internal(T),
}

/// In-flight requests toward one upstream, keyed by minted id.
///
/// `T` is the internal-request payload (a reply channel in production,
/// something inspectable in tests).
#[derive(Debug)]
pub struct PendingMap<T> {
    next_id: u64,
    pending: HashMap<u64, Pending<T>>,
    by_client: HashMap<RequestId, u64>,
}

impl<T> Default for PendingMap<T> {
    fn default() -> Self {
        Self {
            next_id: 0,
            pending: HashMap::new(),
            by_client: HashMap::new(),
        }
    }
}

impl<T> PendingMap<T> {
    /// Mints an upstream id for a forwarded client request.
    ///
    /// The router's [`ClientTable`] has already rejected duplicate
    /// client ids session-wide, so a collision here cannot happen; if a
    /// stale mapping existed anyway it is replaced, orphaning the old
    /// entry (its response will arrive unmapped and be dropped).
    pub fn insert_client(
        &mut self,
        client_id: RequestId,
        client_id_raw: &str,
        call_id: CallId,
    ) -> u64 {
        let id = self.mint();
        if let Some(stale) = self.by_client.insert(client_id.clone(), id) {
            self.pending.remove(&stale);
        }
        self.pending.insert(
            id,
            Pending::Client {
                client_id,
                client_id_raw: client_id_raw.into(),
                call_id,
            },
        );
        id
    }

    /// Mints an upstream id for a proxy-internal request.
    pub fn insert_internal(&mut self, payload: T) -> u64 {
        let id = self.mint();
        self.pending.insert(id, Pending::Internal(payload));
        id
    }

    /// Consumes the pending entry a response answers. `None` means the
    /// id is unknown — a late response after cancel, a duplicate, or an
    /// id the upstream invented — and the response must be dropped.
    pub fn complete(&mut self, upstream_id: u64) -> Option<Pending<T>> {
        let entry = self.pending.remove(&upstream_id)?;
        if let Pending::Client { client_id, .. } = &entry {
            self.by_client.remove(client_id);
        }
        Some(entry)
    }

    /// Forgets a forwarded client request because the client cancelled
    /// it, returning the minted upstream id to rewrite into the
    /// forwarded cancellation. `None` means the request is not in
    /// flight (already answered, or never forwarded) and the
    /// cancellation should be dropped.
    pub fn cancel_client(&mut self, client_id: &RequestId) -> Option<u64> {
        let id = self.by_client.remove(client_id)?;
        self.pending.remove(&id);
        Some(id)
    }

    /// Drains every pending entry, for teardown: internal requesters
    /// get their payloads back (to be failed), client entries are
    /// simply dropped with the session.
    pub fn drain(&mut self) -> Vec<Pending<T>> {
        self.by_client.clear();
        self.pending.drain().map(|(_, entry)| entry).collect()
    }

    /// Number of in-flight requests.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// True when nothing is in flight.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn mint(&mut self) -> u64 {
        let id = self.next_id;
        // Even at a call per nanosecond this cannot wrap within the
        // lifetime of a session; wrapping_add keeps the impossible case
        // panic-free anyway.
        self.next_id = self.next_id.wrapping_add(1);
        id
    }
}

/// One client `tools/call` in flight, as the router remembers it.
///
/// The tool name and the [`CallId`] ride along because the events that
/// close a call out — a completion, a cancellation, an abandonment at
/// teardown — are emitted where only the client id is known. Without them
/// a `CallCompleted` could not name the call its `CallDecided` opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    /// Which upstream is serving it.
    pub upstream: usize,
    /// The tool that was called, as the client named it.
    pub tool: String,
    /// The correlation id its decision event carries.
    pub call_id: CallId,
}

/// The router's session-wide table of client requests in flight, mapping
/// each client id to what the router knows about that call.
#[derive(Debug, Default)]
pub struct ClientTable {
    routes: HashMap<RequestId, InFlight>,
}

/// Rejection returned when a client reuses an id that is still in
/// flight; JSON-RPC requires ids to be unique among outstanding
/// requests, and answering both would desynchronize the session.
#[derive(Debug, PartialEq, Eq)]
pub struct DuplicateId;

impl ClientTable {
    /// Records a client request as in flight.
    ///
    /// # Errors
    ///
    /// [`DuplicateId`] when that id is already in flight. The router
    /// claims the id *before* asking for a decision, so a reused id can
    /// never produce an `Allow` and then a refusal for the same call.
    pub fn insert(&mut self, client_id: RequestId, call: InFlight) -> Result<(), DuplicateId> {
        match self.routes.entry(client_id) {
            std::collections::hash_map::Entry::Occupied(_) => Err(DuplicateId),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(call);
                Ok(())
            }
        }
    }

    /// Removes a completed request. `None` if it was not in flight
    /// (already removed by a cancellation racing the response).
    pub fn remove(&mut self, client_id: &RequestId) -> Option<InFlight> {
        self.routes.remove(client_id)
    }

    /// Which upstream serves an in-flight request, without removing it.
    pub fn route(&self, client_id: &RequestId) -> Option<&InFlight> {
        self.routes.get(client_id)
    }

    /// Is this client id already in flight?
    ///
    /// Asked before the tool table is consulted, so that the answer to a
    /// reused id is the same whatever tool it names — otherwise the two
    /// refusal codes would tell a client which tools its upstreams offer.
    pub fn contains(&self, client_id: &RequestId) -> bool {
        self.routes.contains_key(client_id)
    }

    /// Everything still in flight, oldest call first, emptying the table.
    ///
    /// Used once, at teardown, to close every open call out as
    /// [`flavium_core::CallOutcome::Abandoned`]; the [`CallId`] order
    /// makes that tail of the trace deterministic.
    pub fn drain(&mut self) -> Vec<InFlight> {
        let mut calls: Vec<InFlight> = self.routes.drain().map(|(_, call)| call).collect();
        calls.sort_by_key(|call| call.call_id);
        calls
    }

    /// Number of client requests in flight.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// True when no client requests are in flight.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cid(s: &str) -> RequestId {
        RequestId::String(s.to_owned())
    }

    #[test]
    fn minted_ids_are_sequential_and_translate_back() {
        let mut map = PendingMap::<()>::default();
        let a = map.insert_client(RequestId::Number(5), "5", CallId(0));
        let b = map.insert_client(cid("x"), "\"x\"", CallId(1));
        assert_eq!((a, b), (0, 1));

        match map.complete(0).unwrap() {
            Pending::Client {
                client_id,
                client_id_raw,
                call_id,
            } => {
                assert_eq!(client_id, RequestId::Number(5));
                assert_eq!(&*client_id_raw, "5");
                assert_eq!(call_id, CallId(0));
            }
            other => panic!("expected client entry, got {other:?}"),
        }
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn internal_and_client_requests_share_one_id_space() {
        let mut map = PendingMap::<&'static str>::default();
        let init = map.insert_internal("init");
        let call = map.insert_client(RequestId::Number(0), "0", CallId(0));
        assert_ne!(init, call);
        assert!(matches!(
            map.complete(init).unwrap(),
            Pending::Internal("init")
        ));
    }

    #[test]
    fn unknown_and_repeated_completions_are_none() {
        let mut map = PendingMap::<()>::default();
        let id = map.insert_client(RequestId::Number(1), "1", CallId(0));
        assert!(map.complete(999).is_none());
        assert!(map.complete(id).is_some());
        // A duplicate response to the same id no longer translates.
        assert!(map.complete(id).is_none());
    }

    #[test]
    fn cancel_removes_the_mapping_so_late_responses_drop() {
        let mut map = PendingMap::<()>::default();
        let id = map.insert_client(cid("req"), "\"req\"", CallId(0));
        assert_eq!(map.cancel_client(&cid("req")), Some(id));
        // The late response finds no mapping: dropped by the caller.
        assert!(map.complete(id).is_none());
        assert!(map.is_empty());
        // Cancelling something unknown is a no-op.
        assert_eq!(map.cancel_client(&cid("req")), None);
    }

    #[test]
    fn drain_returns_everything_pending() {
        let mut map = PendingMap::<u8>::default();
        map.insert_internal(1);
        map.insert_client(RequestId::Number(2), "2", CallId(0));
        let drained = map.drain();
        assert_eq!(drained.len(), 2);
        assert!(map.is_empty());
    }

    fn call(upstream: usize, tool: &str, call_id: u64) -> InFlight {
        InFlight {
            upstream,
            tool: tool.to_owned(),
            call_id: CallId(call_id),
        }
    }

    #[test]
    fn client_table_rejects_duplicate_inflight_ids() {
        let mut table = ClientTable::default();
        table
            .insert(RequestId::Number(1), call(0, "read_file", 1))
            .unwrap();
        assert_eq!(
            table.insert(RequestId::Number(1), call(1, "send_mail", 2)),
            Err(DuplicateId)
        );
        assert!(table.contains(&RequestId::Number(1)));
        assert!(!table.contains(&RequestId::Number(2)));
        assert_eq!(
            table.route(&RequestId::Number(1)),
            Some(&call(0, "read_file", 1))
        );
        assert_eq!(
            table.remove(&RequestId::Number(1)),
            Some(call(0, "read_file", 1))
        );
        assert!(!table.contains(&RequestId::Number(1)));

        // Once answered, the id may be reused — with a *new* CallId, so
        // a queued response to the old call no longer matches it.
        table
            .insert(RequestId::Number(1), call(1, "send_mail", 3))
            .unwrap();
        let routed = table.route(&RequestId::Number(1)).unwrap();
        assert_eq!(routed.upstream, 1);
        assert_eq!(routed.call_id, CallId(3));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn drain_returns_open_calls_in_call_id_order() {
        let mut table = ClientTable::default();
        table.insert(cid("c"), call(0, "c_tool", 30)).unwrap();
        table.insert(cid("a"), call(1, "a_tool", 10)).unwrap();
        table.insert(cid("b"), call(0, "b_tool", 20)).unwrap();
        let drained = table.drain();
        assert_eq!(
            drained.iter().map(|c| c.call_id.0).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        assert_eq!(drained[0].tool, "a_tool");
        assert!(table.is_empty());
        assert!(table.drain().is_empty());
    }
}
