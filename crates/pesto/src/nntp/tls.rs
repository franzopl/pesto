//! Shared rustls client configuration.

use std::sync::{Arc, OnceLock};

use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// Build the rustls client configuration, trusting the bundled Mozilla roots.
/// Build (once) and share the TLS client config across every connection.
///
/// Building this from scratch — populating a `RootCertStore` with 100+
/// webpki root certificates and constructing a fresh crypto provider — is
/// synchronous, non-trivial CPU work with no `.await` point in it. Doing
/// that on *every* `connect()` call is harmless for one connection at a
/// time, but opening many connections concurrently (e.g. `penne --stat`
/// with a large `connections` count) used to mean that many threads all
/// doing this rebuild at once, blocking the tokio runtime's worker threads
/// long enough to visibly stall progress reporting before any actual NNTP
/// traffic had even started. Building it once and sharing the `Arc` makes
/// every connection after the first pay only a refcount bump.
pub(super) fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

            let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
            let config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("TLS protocol version configuration is static and always valid")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}
