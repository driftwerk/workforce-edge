//! rustls is compiled with both ring and aws-lc-rs in this dependency graph,
//! so it cannot choose a process-wide default crypto provider by itself. The
//! binary installs one first thing in `main`; this pins that, once it has,
//! the plain `ClientConfig::builder()` that mysql_async, async-nats and
//! hyper-rustls' native-roots builder call no longer panics.

#[test]
fn a_default_client_config_builds_once_the_provider_is_installed() {
    workforce_common::tls::install_default_crypto_provider();
    let provider =
        rustls::crypto::CryptoProvider::get_default().expect("a default provider is installed");
    assert_eq!(
        provider.cipher_suites,
        rustls::crypto::ring::default_provider().cipher_suites,
        "the default is ring",
    );
    let _config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
}
