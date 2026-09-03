//! reqwest is built without a bundled TLS provider (no aws-lc / cmake); install ring once.

use std::sync::Once;

static INIT: Once = Once::new();

/// Install ring as the process-wide rustls crypto provider. Safe to call repeatedly.
pub fn init() {
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
