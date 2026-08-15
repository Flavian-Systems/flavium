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
    pub fn insert_client(&mut self, client_id: RequestId, client_id_raw: &str) -> u64 {
        let id = self.mint();
        if let Some(stale) = self.by_client.insert(client_id.clone(), id) {
            self.pending.remove(&stale);
        }
        self.pending.insert(
            id,
            Pending::Client {
                client_id,
                client_id_raw: client_id_raw.into(),
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

/// The router's session-wide table of client requests in flight, mapping
/// each client id to the upstream serving it.
#[derive(Debug, Default)]
pub struct ClientTable {
    routes: HashMap<RequestId, usize>,
}

/// Rejection returned when a client reuses an id that is still in
/// flight; JSON-RPC requires ids to be unique among outstanding
/// requests, and answering both would desynchronize the session.
#[derive(Debug, PartialEq, Eq)]
pub struct DuplicateId;

impl ClientTable {
    /// Records a client request as in flight toward `upstream`.
    pub fn insert(&mut self, client_id: RequestId, upstream: usize) -> Result<(), DuplicateId> {
        match self.routes.entry(client_id) {
            std::collections::hash_map::Entry::Occupied(_) => Err(DuplicateId),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(upstream);
                Ok(())
            }
        }
    }

    /// Removes a completed request. `None` if it was not in flight
    /// (already removed by a cancellation racing the response).
    pub fn remove(&mut self, client_id: &RequestId) -> Option<usize> {
        self.routes.remove(client_id)
    }

    /// Which upstream serves an in-flight request, without removing it.
    pub fn route(&self, client_id: &RequestId) -> Option<usize> {
        self.routes.get(client_id).copied()
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
        let a = map.insert_client(RequestId::Number(5), "5");
        let b = map.insert_client(cid("x"), "\"x\"");
        assert_eq!((a, b), (0, 1));

        match map.complete(0).unwrap() {
            Pending::Client {
                client_id,
                client_id_raw,
            } => {
                assert_eq!(client_id, RequestId::Number(5));
                assert_eq!(&*client_id_raw, "5");
            }
            other => panic!("expected client entry, got {other:?}"),
        }
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn internal_and_client_requests_share_one_id_space() {
        let mut map = PendingMap::<&'static str>::default();
        let init = map.insert_internal("init");
        let call = map.insert_client(RequestId::Number(0), "0");
        assert_ne!(init, call);
        assert!(matches!(
            map.complete(init).unwrap(),
            Pending::Internal("init")
        ));
    }

    #[test]
    fn unknown_and_repeated_completions_are_none() {
        let mut map = PendingMap::<()>::default();
        let id = map.insert_client(RequestId::Number(1), "1");
        assert!(map.complete(999).is_none());
        assert!(map.complete(id).is_some());
        // A duplicate response to the same id no longer translates.
        assert!(map.complete(id).is_none());
    }

    #[test]
    fn cancel_removes_the_mapping_so_late_responses_drop() {
        let mut map = PendingMap::<()>::default();
        let id = map.insert_client(cid("req"), "\"req\"");
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
        map.insert_client(RequestId::Number(2), "2");
        let drained = map.drain();
        assert_eq!(drained.len(), 2);
        assert!(map.is_empty());
    }

    #[test]
    fn client_table_rejects_duplicate_inflight_ids() {
        let mut table = ClientTable::default();
        table.insert(RequestId::Number(1), 0).unwrap();
        assert_eq!(table.insert(RequestId::Number(1), 1), Err(DuplicateId));
        assert_eq!(table.route(&RequestId::Number(1)), Some(0));
        assert_eq!(table.remove(&RequestId::Number(1)), Some(0));
        // Once answered, the id may be reused.
        table.insert(RequestId::Number(1), 1).unwrap();
        assert_eq!(table.route(&RequestId::Number(1)), Some(1));
    }
}
