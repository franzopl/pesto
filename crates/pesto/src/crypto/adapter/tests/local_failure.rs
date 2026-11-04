//! Local (non-provider) crypto failures are typed `LocalCryptoError`
//! (PESTO13-01): penne aborts on that type instead of failing over.

use super::super::super::kdf::LocalCryptoError;
use super::fixture_kdf_input;
use super::*;

#[test]
fn failing_derive_is_local_crypto_error() {
    let adapter = DownloadDecryptionAdapter::with_password(fixture_kdf_input());
    let err = adapter
        .get_or_derive_session([0x41; 16], || {
            anyhow::bail!("argon2 kdf computation failed")
        })
        .expect_err("failing derivation must fail");
    assert!(
        err.is::<LocalCryptoError>(),
        "derivation failure must be typed LocalCryptoError, got: {err:#}"
    );
    // A failed derivation stays cached: a second lookup returns the same
    // typed failure without re-running the derivation.
    let err2 = adapter
        .get_or_derive_session([0x41; 16], || {
            let salt = [0x41; 16];
            Ok(EncryptionSession::new(fixture_kdf_input(), salt).unwrap())
        })
        .expect_err("cached derivation failure must stay cached");
    assert!(err2.is::<LocalCryptoError>(), "got: {err2:#}");
}

#[test]
fn poisoned_cache_is_local_crypto_error() {
    let adapter = DownloadDecryptionAdapter::with_password(fixture_kdf_input());
    adapter.poison_session_cache_for_tests();
    let err = adapter
        .get_or_create_session([0x42; 16])
        .expect_err("poisoned session cache must fail");
    assert!(
        err.is::<LocalCryptoError>(),
        "poisoned cache must be typed LocalCryptoError, got: {err:#}"
    );
}
