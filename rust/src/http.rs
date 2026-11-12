use std::sync::Once;
use std::time::Duration;

/// TLS needs a crypto provider installed once per process. `ring` builds without a C++ toolchain,
/// which keeps cross-compiling for the Raspberry simple.
pub fn install_crypto() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// The one HTTP client shared by discovery, scores and upstream calls.
pub fn build_client(connect_timeout_secs: f64) -> reqwest::Client {
    install_crypto();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs_f64(connect_timeout_secs))
        .pool_max_idle_per_host(2)
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .expect("HTTP client")
}
