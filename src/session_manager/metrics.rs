//! Prometheus exporter for session-manager metrics.
//!
//! После урезания менеджера до агрегатора spawn/kill-счётчики удалены вместе
//! с соответствующими механизмами. Остался только установщик Prometheus
//! exporter — на тот случай, если будущие компоненты (notify, registry,
//! transport) захотят регистрировать свои метрики через `metrics` crate.

/// Install a Prometheus metrics exporter bound to `bind_address`.
///
/// Returns an error string if binding or installation fails.
/// This function is idempotent: calling it more than once will return an error
/// from the metrics crate (only one global recorder is allowed).
pub fn install_prometheus_exporter(bind_address: &str) -> Result<(), String> {
    use metrics_exporter_prometheus::PrometheusBuilder;
    PrometheusBuilder::new()
        .with_http_listener(
            bind_address
                .parse::<std::net::SocketAddr>()
                .map_err(|e| format!("invalid metrics bind_address '{bind_address}': {e}"))?,
        )
        .install()
        .map_err(|e| format!("failed to install Prometheus exporter: {e}"))
}
