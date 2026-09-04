//! hyper-rustls and reqwest are built without a bundled TLS provider (no aws-lc / cmake).

/// Install ring as the process-wide rustls crypto provider. Safe to call repeatedly: a
/// second install is refused and the error ignored.
pub fn init() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
