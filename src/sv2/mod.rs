//! Stratum V2 native pool integration.
//!
//! This module owns the native SV2 listener and protocol state machine. The
//! first implementation target is the Mining subprotocol with Extended
//! Channels; Standard Channels require channel-specific fixed merkle roots and
//! are intentionally handled separately.
//!
//! Scope (Q2 = Pool only): only the SV2 Mining subprotocol will be served.
//! Job Declaration and SV2 Template Provider are out of scope; templates
//! continue to come from `crate::template`.

mod connection;
mod keys;
mod transport;

use std::sync::Arc;

use crate::config::Config;
use crate::metrics::MetricsStore;
use crate::template::TemplateEngine;

pub use transport::Sv2Server;

pub async fn run(
    config: Config,
    template_engine: Arc<TemplateEngine>,
    metrics: MetricsStore,
) -> anyhow::Result<()> {
    Sv2Server::new(config, template_engine, metrics)?
        .run()
        .await
}

pub fn log_settings(config: &Config) {
    tracing::debug!(
        bind = %config.sv2_bind,
        port = config.sv2_port,
        max_connections = config.sv2_max_connections,
        max_connections_per_ip = config.sv2_max_connections_per_ip,
        max_handshakes = config.sv2_max_handshakes,
        handshake_timeout_secs = config.sv2_handshake_timeout_secs,
        idle_timeout_secs = config.sv2_idle_timeout_secs,
        frame_max_bytes = config.sv2_frame_max_bytes,
        cert_validity_secs = config.sv2_cert_validity_secs,
        "sv2 settings"
    );
}
