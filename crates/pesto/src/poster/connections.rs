//! Connection-budget accounting and slot ownership for one posting run.

use std::sync::Arc;

use anyhow::{bail, Result};

use crate::config::{Config, ServerEntry};
use crate::nntp::pool::{ConnectionBroker, ConnectionPool, ConnectionSlot};

/// Split the configured total between upload workers and the check queue.
///
/// Both automatic and explicit check counts are carved out of the total, so
/// the configured connection count remains a hard upper bound. At least one
/// upload connection is retained whenever checking is enabled.
pub(super) fn split_connections(config: &Config, check_enabled: bool) -> Result<(usize, usize)> {
    let total_conns = config.total_connections();
    if !check_enabled {
        return Ok((0, total_conns));
    }
    let check = if config.check_connections == 0 {
        config
            .effective_check_connections()
            .min(total_conns.saturating_sub(1))
    } else {
        config.check_connections.min(total_conns.saturating_sub(1))
    };
    if check == 0 {
        bail!(
            "checking is enabled but no connection remains for the STAT pool \
             (need at least one upload connection and one check connection). \
             Raise `-n`/`connections`, lower `--check-connections`, or pass `--no-check`"
        );
    }
    Ok((check, total_conns.saturating_sub(check)))
}

/// Check out `n` broker slots, or build a fresh pool when there is no broker.
pub(super) async fn take_slots(
    broker: Option<&Arc<ConnectionBroker>>,
    servers: Arc<Vec<ServerEntry>>,
    n: usize,
) -> Vec<ConnectionSlot> {
    if n == 0 {
        return Vec::new();
    }
    match broker {
        Some(broker) => broker.checkout(n).await,
        None => ConnectionPool::build(servers, n).into_slots(),
    }
}

/// Return the complete slot set to its broker, or close run-owned sockets.
pub(super) async fn release_slots(broker: Option<&ConnectionBroker>, slots: Vec<ConnectionSlot>) {
    match broker {
        Some(broker) => broker.checkin_all(slots).await,
        None => {
            for mut slot in slots {
                slot.quit().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FileConfig, Overrides};

    fn config_with_connections(connections: usize) -> Config {
        let mut file = FileConfig::default();
        file.posting.groups = Some(vec!["alt.test".into()]);
        Config::resolve(
            file,
            Overrides {
                connections: Some(connections),
                dry_run: Some(true),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn automatic_check_pool_is_carved_out_of_the_total() {
        let mut config = config_with_connections(50);
        config.check_connections = 0;
        let (check, upload) = split_connections(&config, true).unwrap();
        assert_eq!((check, upload), (4, 46));
        assert_eq!(check + upload, 50);
    }

    #[test]
    fn disabled_checking_leaves_the_whole_total_for_upload() {
        let config = config_with_connections(50);
        assert_eq!(split_connections(&config, false).unwrap(), (0, 50));
    }

    #[test]
    fn one_connection_with_checking_is_a_startup_error() {
        let mut config = config_with_connections(1);
        config.check_connections = 0;
        let error = split_connections(&config, true).unwrap_err().to_string();
        assert!(error.contains("--no-check"));
        assert!(error.contains("-n") || error.contains("connections"));
    }

    #[test]
    fn explicit_check_count_is_carved_out_and_clamped() {
        let mut config = config_with_connections(10);
        config.check_connections = 4;
        assert_eq!(split_connections(&config, true).unwrap(), (4, 6));

        config.check_connections = 10;
        assert_eq!(split_connections(&config, true).unwrap(), (9, 1));

        config.connections = 1;
        config.check_connections = 1;
        let error = split_connections(&config, true).unwrap_err().to_string();
        assert!(error.contains("--no-check"));
    }

    #[test]
    fn small_total_keeps_one_upload_connection() {
        let mut config = config_with_connections(2);
        config.check_connections = 0;
        assert_eq!(split_connections(&config, true).unwrap(), (1, 1));
    }

    #[test]
    fn automatic_pool_favors_upload_for_small_totals() {
        let mut config = config_with_connections(4);
        config.check_connections = 0;
        assert_eq!(split_connections(&config, true).unwrap(), (1, 3));
    }

    #[test]
    fn automatic_pool_is_capped_for_large_totals() {
        let mut config = config_with_connections(200);
        config.check_connections = 0;
        assert_eq!(split_connections(&config, true).unwrap(), (4, 196));
    }
}
