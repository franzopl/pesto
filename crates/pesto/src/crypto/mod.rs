//! Cryptographic layer for yEnc body and control-line encryption.
//!
//! Implements the experimental yEnc encryption standards v1.2:
//! - **Body Encryption:** Per-segment XChaCha20-Poly1305 AEAD before yEnc encoding.
//! - **Control-Line Encryption:** Radix 253 NIST SP 800-38G FF1 format-preserving encryption
//!   of all `=ybegin`, `=ypart`, `=yend`, and `=yencryption` control lines.
//! - **Key Derivation:** Argon2id (RFC 9106, t=1, m=64MB, p=4) with a 16-byte random session salt.
//!
//! See the root README's Encryption section for the wire format, threat
//! model, and protocol status.
//!
//! **Operational constraint (v1.2):** per-segment identity (salt +
//! globally unique `segmentIndex`) is carried exclusively in the 20-byte
//! bootstrap prefix of encrypted control Line 1 and the `=yencryption`
//! line — never in NZB XML attributes.

pub(crate) mod adapter;
pub(crate) mod body;
pub(crate) mod control;
pub(crate) mod kdf;
pub(crate) mod ybegin;

#[cfg(test)]
mod tests;

pub use adapter::{DownloadDecryptionAdapter, UploadEncryptionAdapter};
pub use control::{
    extract_bootstrap_from_line1, generate_alphabet_salt, split_lines_preserving_endings,
};
pub use kdf::{EncryptionSession, LocalCryptoError};

// Error classification note (yEnc Body Encryption Standard §5 Two-Tier
// model; Control Std §8): crypto failures carry their classification in the
// message itself. Callers route provider-caused decode/decrypt failures to
// provider failover (penne's corrupt-copy path), while a typed
// `LocalCryptoError` (local Argon2id/cache failure, never provider-caused)
// aborts the job at once. NZB parse failures are structural and already
// fatal before any network access. The wire error-code strings (e.g.
// `UNSUPPORTED_CIPHER`, `CONTROL_LINE_DECRYPT_FAILURE`) are conformance
// surface matched by the malformed-input vectors and MUST NOT change.
