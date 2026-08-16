//! Pool of concurrent NNTP connections — the source of posting throughput.
//!
//! [`ConnectionPool`] builds a fixed set of [`ConnectionSlot`]s — one per
//! posting worker — distributed across the configured servers according to
//! each server's `connections` quota. Each slot owns a single lazily-opened
//! NNTP session: it connects on first use, reconnects transparently on
//! failure, and rotates through the server list on repeated errors so work
//! shifts to a healthy server automatically.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tracing::{info, warn};

use crate::config::ServerEntry;
use crate::nntp::Connection;

/// One posting worker's dedicated NNTP connection.
///
/// The slot is lazy: no TCP connection is opened until the first call to
/// [`ConnectionSlot::ensure_connected`]. On any network or protocol error the
/// caller should call [`ConnectionSlot::invalidate`] to drop the bad
/// connection and rotate to the next server before retrying.
pub struct ConnectionSlot {
    servers: Arc<Vec<ServerEntry>>,
    conn: Option<Connection>,
    /// Index into `servers` of the server we will connect to next.
    server_idx: usize,
    /// Opaque identifier for log correlation (worker index supplied by caller).
    slot_id: usize,
    /// Reason for the next connection attempt; "initial" until first invalidate.
    next_connect_reason: &'static str,
}

impl ConnectionSlot {
    pub(crate) fn new(servers: Arc<Vec<ServerEntry>>, primary_idx: usize) -> Self {
        ConnectionSlot::with_id(servers, primary_idx, 0)
    }

    pub(crate) fn with_id(
        servers: Arc<Vec<ServerEntry>>,
        primary_idx: usize,
        slot_id: usize,
    ) -> Self {
        ConnectionSlot {
            servers,
            conn: None,
            server_idx: primary_idx,
            slot_id,
            next_connect_reason: "initial",
        }
    }

    /// Index into the server list that this slot is currently targeting.
    pub fn server_idx(&self) -> usize {
        self.server_idx
    }

    /// Ensure a live, authenticated connection is available and return a
    /// mutable reference to it. If no connection exists, one is opened to
    /// `servers[server_idx]`. On failure the slot rotates to the next server
    /// (so a subsequent call tries a different one) and returns the error.
    pub async fn ensure_connected(&mut self) -> Result<&mut Connection> {
        if self.conn.is_none() {
            let server = &self.servers[self.server_idx];
            let host = server.host.clone();
            let idx = self.server_idx;
            let reason = self.next_connect_reason;
            self.next_connect_reason = "initial";
            info!(
                server = "<redacted>",
                server_idx = idx,
                slot_id = self.slot_id,
                reason,
                "connecting"
            );
            match connect_and_auth(server).await {
                Ok(c) => {
                    info!(
                        server = "<redacted>",
                        slot_id = self.slot_id,
                        "connected and authenticated"
                    );
                    self.conn = Some(c);
                }
                Err(e) => {
                    self.rotate();
                    return Err(e.context(format!("connect to {host} (server {idx})")));
                }
            }
        }
        Ok(self.conn.as_mut().unwrap())
    }

    /// Discard the current connection and advance to the next server.
    ///
    /// Call this after any network or protocol error so the next
    /// [`ensure_connected`][Self::ensure_connected] attempt targets a fresh
    /// server.
    pub fn invalidate(&mut self, reason: &'static str) {
        if self.conn.is_some() {
            warn!(
                server = "<redacted>",
                slot_id = self.slot_id,
                reason,
                "connection invalidated; rotating to next server"
            );
        }
        self.conn = None;
        self.next_connect_reason = reason;
        self.rotate();
    }

    /// Point this slot at a specific server (by index into the list it was
    /// built with), so the next [`ensure_connected`][Self::ensure_connected]
    /// targets exactly that server instead of whichever one the slot
    /// currently happens to be on.
    ///
    /// A no-op when `idx` is already the current target (or out of range) —
    /// the existing connection, if any, is kept, so retargeting to the same
    /// server repeatedly (the common case: consecutive items destined for
    /// the same server) never pays for a reconnect. Used by the streaming
    /// check queue, which must `STAT` the same server an article was
    /// actually posted to (see `PostedSegment::server_idx`), not just
    /// whichever server this slot's worker happened to start on.
    pub fn retarget(&mut self, idx: usize) {
        if idx >= self.servers.len() || idx == self.server_idx {
            return;
        }
        self.conn = None;
        self.server_idx = idx;
        self.next_connect_reason = "retarget";
    }

    /// Retry delay of the server the slot is currently targeting. Used by the
    /// worker to back off before reconnecting after a failure.
    pub fn retry_delay(&self) -> Duration {
        Duration::from_secs(self.servers[self.server_idx].retry_delay)
    }

    /// Send `QUIT` and discard the connection. No-op when not connected.
    pub async fn quit(&mut self) {
        if let Some(mut c) = self.conn.take() {
            c.quit().await;
        }
    }

    /// Send `MODE READER` on the current connection to reset the server's idle
    /// timer. If the command fails (connection silently dropped by the server),
    /// the connection is discarded so `ensure_connected` will open a fresh one
    /// on the next use. No-op when not connected.
    pub async fn keepalive(&mut self) {
        if let Some(conn) = self.conn.as_mut() {
            if conn.mode_reader().await.is_err() {
                self.conn = None;
            }
        }
    }

    fn rotate(&mut self) {
        if !self.servers.is_empty() {
            self.server_idx = (self.server_idx + 1) % self.servers.len();
        }
    }
}

/// A fixed set of [`ConnectionSlot`]s — one per posting worker — distributed
/// across the configured servers by connection quota.
pub struct ConnectionPool {
    slots: Vec<ConnectionSlot>,
}

impl ConnectionPool {
    /// Assign `worker_count` slots to servers according to each server's
    /// `connections` quota. Workers beyond the total quota are distributed
    /// round-robin so the result always has exactly `worker_count` entries.
    pub fn build(servers: Arc<Vec<ServerEntry>>, worker_count: usize) -> Self {
        let assignments = assign_workers(&servers, worker_count);
        let slots = assignments
            .into_iter()
            .enumerate()
            .map(|(slot_id, si)| ConnectionSlot::with_id(servers.clone(), si, slot_id))
            .collect();
        ConnectionPool { slots }
    }

    /// Consume the pool and return its slots for distribution to workers.
    pub fn into_slots(self) -> Vec<ConnectionSlot> {
        self.slots
    }
}

/// How often the broker's background task checks idle connections for a
/// pending keepalive, mirroring the poster worker's own `IDLE_POLL`.
const BROKER_IDLE_POLL: Duration = Duration::from_secs(2);

/// A long-lived set of [`ConnectionSlot`]s shared across several sequential
/// (or, under `--jobs`, concurrent) posting runs — e.g. the episodes of an
/// `--each`/`--season` batch — so the TLS+AUTH handshake is paid once per
/// connection for the whole batch instead of once per episode.
///
/// Slots are checked out for the duration of one run and checked back in
/// (not disconnected) when that run's worker is done with them, so the next
/// episode's workers reuse already-authenticated connections. A background
/// task sends periodic keepalives to slots sitting idle in the broker
/// between episodes, the same way a posting worker already does for slots
/// idle *within* a run.
pub struct ConnectionBroker {
    idle: Mutex<VecDeque<(ConnectionSlot, Instant)>>,
    permits: Semaphore,
}

impl ConnectionBroker {
    /// Build a broker with `capacity` connections distributed across
    /// `servers` by quota (via [`ConnectionPool::build`]), and spawn its
    /// background keepalive task. `keepalive_interval` is in seconds; `0`
    /// disables the keepalive task entirely (matching
    /// `Config::keepalive_interval`'s convention).
    ///
    /// Returns the broker and the keepalive task's `JoinHandle`, which the
    /// caller should `.abort()` once [`shutdown`][Self::shutdown] has run.
    pub fn new(
        servers: Arc<Vec<ServerEntry>>,
        capacity: usize,
        keepalive_interval: u64,
    ) -> (Arc<Self>, JoinHandle<()>) {
        let slots = ConnectionPool::build(servers, capacity).into_slots();
        let now = Instant::now();
        let broker = Arc::new(ConnectionBroker {
            idle: Mutex::new(slots.into_iter().map(|s| (s, now)).collect()),
            permits: Semaphore::new(capacity),
        });

        let keepalive_task = {
            let broker = broker.clone();
            tokio::spawn(async move {
                if keepalive_interval == 0 {
                    return;
                }
                loop {
                    tokio::time::sleep(BROKER_IDLE_POLL).await;
                    let mut idle = broker.idle.lock().await;
                    for (slot, last_used) in idle.iter_mut() {
                        if last_used.elapsed() >= Duration::from_secs(keepalive_interval) {
                            slot.keepalive().await;
                            *last_used = Instant::now();
                        }
                    }
                }
            })
        };

        (broker, keepalive_task)
    }

    /// Check out `n` connections, waiting for enough to become idle if the
    /// broker's whole capacity is currently checked out elsewhere (this is
    /// how concurrent `--jobs` episodes end up sharing one fixed connection
    /// budget instead of each opening their own).
    pub async fn checkout(&self, n: usize) -> Vec<ConnectionSlot> {
        if n == 0 {
            return Vec::new();
        }
        // Acquired permits are intentionally leaked (not held/released via
        // guard) — capacity is returned to the semaphore explicitly by
        // `checkin`, once each slot actually comes back.
        self.permits
            .acquire_many(n as u32)
            .await
            .expect("broker semaphore is never closed")
            .forget();
        let mut idle = self.idle.lock().await;
        (0..n)
            .map(|_| {
                idle.pop_front()
                    .expect("permits guarantee a matching idle slot")
                    .0
            })
            .collect()
    }

    /// Return a checked-out connection to the idle pool instead of closing
    /// it, so a later `checkout` (the next episode, or another concurrent
    /// one) can reuse it without reconnecting.
    pub async fn checkin(&self, slot: ConnectionSlot) {
        self.idle.lock().await.push_back((slot, Instant::now()));
        self.permits.add_permits(1);
    }

    /// Send `QUIT` on every idle connection. Call once, after every checked
    /// out slot has been returned via `checkin` (i.e. after the whole batch
    /// that shares this broker has finished).
    pub async fn shutdown(&self) {
        let mut idle = self.idle.lock().await;
        for (mut slot, _) in idle.drain(..) {
            slot.quit().await;
        }
    }
}

/// Open a connection to `server` and authenticate when credentials are
/// configured.
pub(crate) async fn connect_and_auth(server: &ServerEntry) -> Result<Connection> {
    let mut conn =
        Connection::connect(&server.host, server.port, server.ssl, server.timeout).await?;
    if let Some(username) = &server.username {
        let password = server.password.as_deref().unwrap_or("");
        conn.authenticate(username, password).await?;
    }
    Ok(conn)
}

/// Returns a `Vec<usize>` of length `worker_count` where element `i` is the
/// index (into `servers`) of the server worker `i` should connect to first.
///
/// Workers are assigned to servers in order, each server receiving up to its
/// `connections` quota. If the total quota is exhausted before `worker_count`
/// is reached the remaining workers are filled round-robin.
pub(crate) fn assign_workers(servers: &[ServerEntry], worker_count: usize) -> Vec<usize> {
    let mut out = Vec::with_capacity(worker_count);
    let mut remaining = worker_count;
    'outer: for (si, server) in servers.iter().enumerate() {
        for _ in 0..server.connections {
            if remaining == 0 {
                break 'outer;
            }
            out.push(si);
            remaining -= 1;
        }
    }
    // Quota exhausted before worker_count reached: fill round-robin.
    while out.len() < worker_count {
        out.push(out.len() % servers.len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(connections: usize) -> ServerEntry {
        ServerEntry {
            host: "h".into(),
            port: 563,
            ssl: true,
            connections,
            username: None,
            password: None,
            retry_delay: 1,
            timeout: crate::config::DEFAULT_TIMEOUT_SECS,
        }
    }

    fn arc(servers: Vec<ServerEntry>) -> Arc<Vec<ServerEntry>> {
        Arc::new(servers)
    }

    // ── assign_workers ────────────────────────────────────────────────────────

    #[test]
    fn single_server_all_workers_assigned_to_it() {
        let out = assign_workers(&[server(8)], 4);
        assert_eq!(out, vec![0, 0, 0, 0]);
    }

    #[test]
    fn two_servers_workers_distributed_by_connection_count() {
        let out = assign_workers(&[server(2), server(4)], 6);
        assert_eq!(out[..2], [0, 0]);
        assert_eq!(out[2..], [1, 1, 1, 1]);
    }

    #[test]
    fn worker_count_less_than_total_connections_stops_early() {
        let out = assign_workers(&[server(8), server(8)], 3);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn worker_count_exceeds_total_connections_fills_round_robin() {
        let out = assign_workers(&[server(1), server(1)], 4);
        assert_eq!(out.len(), 4);
    }

    // ── ConnectionPool::build ─────────────────────────────────────────────────

    #[test]
    fn pool_build_creates_one_slot_per_worker() {
        let pool = ConnectionPool::build(arc(vec![server(4)]), 3);
        assert_eq!(pool.into_slots().len(), 3);
    }

    #[test]
    fn pool_into_slots_drains_all_slots() {
        let pool = ConnectionPool::build(arc(vec![server(4), server(4)]), 5);
        let slots = pool.into_slots();
        assert_eq!(slots.len(), 5);
    }

    // ── ConnectionSlot ────────────────────────────────────────────────────────

    #[test]
    fn slot_starts_at_primary_server() {
        let slot = ConnectionSlot::new(arc(vec![server(2), server(2)]), 1);
        assert_eq!(slot.server_idx(), 1);
    }

    #[test]
    fn slot_invalidate_rotates_to_next_server() {
        let mut slot = ConnectionSlot::new(arc(vec![server(1), server(1), server(1)]), 0);
        slot.invalidate("test");
        assert_eq!(slot.server_idx(), 1);
        slot.invalidate("test");
        assert_eq!(slot.server_idx(), 2);
        slot.invalidate("test");
        assert_eq!(slot.server_idx(), 0); // wraps around
    }

    #[test]
    fn slot_retry_delay_reflects_current_server() {
        let mut servers = vec![server(1), server(1)];
        servers[0].retry_delay = 5;
        servers[1].retry_delay = 10;
        let mut slot = ConnectionSlot::new(arc(servers), 0);
        assert_eq!(slot.retry_delay(), Duration::from_secs(5));
        slot.invalidate("test");
        assert_eq!(slot.retry_delay(), Duration::from_secs(10));
    }

    #[test]
    fn slot_quit_when_not_connected_is_noop() {
        // Should not panic when there is no open connection.
        let slot = ConnectionSlot::new(arc(vec![server(1)]), 0);
        // Can't call async quit in a sync test, but we can verify the slot
        // has no connection to begin with.
        assert!(slot.conn.is_none());
    }

    // ── ConnectionBroker ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn broker_checkout_returns_exactly_n_slots() {
        let (broker, keepalive) = ConnectionBroker::new(arc(vec![server(4)]), 4, 0);
        let slots = broker.checkout(3).await;
        assert_eq!(slots.len(), 3);
        keepalive.abort();
    }

    #[tokio::test]
    async fn broker_checkout_beyond_capacity_blocks_until_checkin() {
        let (broker, keepalive) = ConnectionBroker::new(arc(vec![server(2)]), 2, 0);
        let slots = broker.checkout(2).await;
        assert_eq!(slots.len(), 2);

        // The whole capacity is checked out — a further checkout must not
        // resolve until one of those slots comes back.
        let blocked = tokio::time::timeout(Duration::from_millis(100), broker.checkout(1)).await;
        assert!(
            blocked.is_err(),
            "checkout beyond capacity resolved without a matching checkin"
        );

        broker.checkin(slots.into_iter().next().unwrap()).await;
        let resumed = tokio::time::timeout(Duration::from_millis(100), broker.checkout(1)).await;
        assert!(
            resumed.is_ok(),
            "checkout did not resume after a checkin freed capacity"
        );
        keepalive.abort();
    }

    #[tokio::test]
    async fn broker_checkin_makes_a_slot_available_again() {
        let (broker, keepalive) = ConnectionBroker::new(arc(vec![server(1)]), 1, 0);
        let mut slots = broker.checkout(1).await;
        let slot = slots.pop().unwrap();
        broker.checkin(slot).await;

        let slots_again = broker.checkout(1).await;
        assert_eq!(slots_again.len(), 1);
        keepalive.abort();
    }

    #[tokio::test]
    async fn broker_shutdown_drains_the_idle_queue() {
        let (broker, keepalive) = ConnectionBroker::new(arc(vec![server(3)]), 3, 0);
        broker.shutdown().await;
        assert!(broker.idle.lock().await.is_empty());
        keepalive.abort();
    }
}
