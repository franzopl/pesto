//! Cryptographic layer for yEnc body and control-line encryption.
//!
//! # Protocol Overview (Experimental)
//!
//! This module implements the experimental yEnc encryption standards:
//! - **Body Encryption:** Per-segment XChaCha20-Poly1305 AEAD before yEnc encoding.
//! - **Control-Line Encryption:** Radix 253 NIST SP 800-38G FF1 format-preserving encryption
//!   of all `=ybegin`, `=ypart`, `=yend`, and `=yencryption` control lines.
//! - **Key Derivation:** Argon2id (RFC 9106, t=1, m=64MB, p=4) with a 16-byte random session salt.
//!
//! # Threat Model
//!
//! - **Confidentiality:** Content encryption at the article layer protects stored Usenet articles
//!   from parties that do not possess the NZB and password. However, because the password is
//!   conventionally distributed via the NZB (`<meta type="password">`), release confidentiality
//!   depends strictly on private distribution of the NZB file.
//! - **Transport Security:** TLS protects network transit to and from NNTP providers;
//!   article-layer encryption protects content at rest on Usenet backend storage.
//! - **Integrity:** Poly1305 tags authenticate segment bodies. Authentication failure releases
//!   zero unauthenticated plaintext and is treated as provider corruption eligible for tier failover.
//!
//! # Operational Constraints
//!
//! - **Season Consolidation:** Multi-session season consolidation into a single NZB is unsupported
//!   under encryption because each upload session uses an independent random salt and a distinct
//!   global `segmentIndex` space. Combining multiple sessions violates segment index uniqueness.
//! - **Status:** The protocol is currently experimental. All KDF parameters, tweak/nonce derivation
//!   rules, control-line formats, and test vectors are frozen for this release. An independent formal
//!   cryptographic review is recommended before stabilization.

pub mod adapter;
pub mod body;
pub mod control;
pub mod kdf;

#[cfg(test)]
mod tests;

pub use adapter::{DownloadDecryptionAdapter, UploadEncryptionAdapter};
pub use kdf::EncryptionSession;
