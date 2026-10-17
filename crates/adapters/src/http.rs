//! One way to build HTTP clients for local engines: no idle connection
//! pooling (a local server may restart or swap under us, and a half-open
//! pooled connection must never stall the runner) and short connect
//! timeouts.

use std::time::Duration;

/// A client for a local engine endpoint.
pub fn local_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default()
}
