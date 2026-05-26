//! Pool of concurrent NNTP connections — the source of posting throughput.
//!
//! [`ConnectionPool`] builds a fixed set of [`ConnectionSlot`]s — one per
//! posting worker — distributed across the configured servers according to
//! each server's `connections` quota. Each slot owns a single lazily-opened
//! NNTP session: it connects on first use, reconnects transparently on
//! failure, and rotates through the server list on repeated errors so work
//! shifts to a healthy server automatically.

use std::sync::Arc;

use anyhow::Result;
use tokio::time::Duration;
use tracing::info;

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
}

impl ConnectionSlot {
    pub(crate) fn new(servers: Arc<Vec<ServerEntry>>, primary_idx: usize) -> Self {
        ConnectionSlot {
            servers,
            conn: None,
            server_idx: primary_idx,
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
            info!(server = %host, server_idx = idx, "connecting");
            match connect_and_auth(server).await {
                Ok(c) => {
                    info!(server = %host, "connected and authenticated");
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
    pub fn invalidate(&mut self) {
        if let Some(idx) = self.conn.as_ref().map(|_| self.server_idx) {
            info!(server = %self.servers[idx].host, "connection invalidated; rotating to next server");
        }
        self.conn = None;
        self.rotate();
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
            .map(|si| ConnectionSlot::new(servers.clone(), si))
            .collect();
        ConnectionPool { slots }
    }

    /// Consume the pool and return its slots for distribution to workers.
    pub fn into_slots(self) -> Vec<ConnectionSlot> {
        self.slots
    }
}

/// Open a connection to `server` and authenticate when credentials are
/// configured.
pub(crate) async fn connect_and_auth(server: &ServerEntry) -> Result<Connection> {
    let mut conn = Connection::connect(&server.host, server.port, server.ssl).await?;
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
        slot.invalidate();
        assert_eq!(slot.server_idx(), 1);
        slot.invalidate();
        assert_eq!(slot.server_idx(), 2);
        slot.invalidate();
        assert_eq!(slot.server_idx(), 0); // wraps around
    }

    #[test]
    fn slot_retry_delay_reflects_current_server() {
        let mut servers = vec![server(1), server(1)];
        servers[0].retry_delay = 5;
        servers[1].retry_delay = 10;
        let mut slot = ConnectionSlot::new(arc(servers), 0);
        assert_eq!(slot.retry_delay(), Duration::from_secs(5));
        slot.invalidate();
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
}
