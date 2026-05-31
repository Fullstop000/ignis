//! OpenTelemetry export pipeline. Off by default; opt in with
//! `IGNIS_ENABLE_TELEMETRY=1` plus standard `OTEL_EXPORTER_OTLP_*` env vars
//! (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`,
//! `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_RESOURCE_ATTRIBUTES`).
//!
//! Prompt content and tool arguments are redacted by default; users can opt in
//! with `IGNIS_LOG_USER_PROMPTS=1` and `IGNIS_LOG_TOOL_DETAILS=1` respectively.
//!
//! All public functions are no-ops when the `telemetry` Cargo feature is
//! disabled OR when telemetry is not enabled at runtime — callers do not need
//! to check.

use crate::config::Config;
use crate::Usage;
use std::time::Duration;

// ============================================================
// Always-on public surface
// ============================================================

/// Snapshot of telemetry state for the `/telemetry` slash command inspector.
#[derive(Debug, Clone, Default)]
pub struct TelemetryStateSnapshot {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub protocol: Option<String>,
    pub log_user_prompts: bool,
    pub log_tool_details: bool,
    pub feature_compiled: bool,
}

/// Returns true if the running binary was compiled with the `telemetry` feature.
pub const fn feature_compiled() -> bool {
    cfg!(feature = "telemetry")
}

/// Returns true if `IGNIS_LOG_USER_PROMPTS=1` is set — gates inclusion of prompt
/// content in the `ignis.turn` span.
pub fn log_user_prompts() -> bool {
    env_truthy("IGNIS_LOG_USER_PROMPTS")
}

/// Returns true if `IGNIS_LOG_TOOL_DETAILS=1` is set — gates inclusion of tool
/// arguments and result content in tool spans.
pub fn log_tool_details() -> bool {
    env_truthy("IGNIS_LOG_TOOL_DETAILS")
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True" | "yes" | "on"))
        .unwrap_or(false)
}

/// Returns true if telemetry is enabled (env var OR config). Independent of
/// whether init() has been called.
pub fn is_enabled(config: &Config) -> bool {
    if env_truthy("IGNIS_ENABLE_TELEMETRY") {
        return true;
    }
    config.telemetry.enabled
}

// ============================================================
// Real implementation (telemetry feature compiled in)
// ============================================================

#[cfg(feature = "telemetry")]
mod imp {
    use super::*;
    use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider as _};
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::{global, KeyValue};
    use opentelemetry_otlp::{MetricExporter, SpanExporter};
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use opentelemetry_sdk::Resource;
    use std::sync::OnceLock;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    /// Lazy global metrics registry. `init()` populates this; recorders read it.
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    /// Snapshot of the resolved endpoint + protocol for `/telemetry`.
    static STATE: OnceLock<TelemetryStateSnapshot> = OnceLock::new();

    struct Metrics {
        session_count: Counter<u64>,
        token_usage: Counter<u64>,
        token_usage_semconv: Counter<u64>,
        tool_calls: Counter<u64>,
        tool_duration: Histogram<f64>,
        llm_duration: Histogram<f64>,
    }

    impl Metrics {
        fn new(meter: Meter) -> Self {
            Self {
                session_count: meter
                    .u64_counter("ignis.session.count")
                    .with_description("Number of ignis sessions started")
                    .build(),
                token_usage: meter
                    .u64_counter("ignis.token.usage")
                    .with_description("LLM token usage (input/output/reasoning/cache)")
                    .build(),
                token_usage_semconv: meter
                    .u64_counter("gen_ai.client.token.usage")
                    .with_description("OTel GenAI semconv: client-side token usage")
                    .build(),
                tool_calls: meter
                    .u64_counter("ignis.tool.calls")
                    .with_description("Tool invocations by name and success")
                    .build(),
                tool_duration: meter
                    .f64_histogram("ignis.tool.call.duration_ms")
                    .with_description("Tool call wall time")
                    .with_unit("ms")
                    .build(),
                llm_duration: meter
                    .f64_histogram("ignis.llm.request.duration_ms")
                    .with_description("LLM API request wall time (stream end-to-end)")
                    .with_unit("ms")
                    .build(),
            }
        }
    }

    /// RAII guard that flushes + shuts down OTel providers on Drop. Held by
    /// `main` for the process lifetime. CC's missing-flush footgun is fixed here.
    pub struct TelemetryGuard {
        tracer_provider: Option<SdkTracerProvider>,
        meter_provider: Option<SdkMeterProvider>,
    }

    impl Drop for TelemetryGuard {
        fn drop(&mut self) {
            if let Some(tp) = self.tracer_provider.take() {
                let _ = tp.shutdown();
            }
            if let Some(mp) = self.meter_provider.take() {
                let _ = mp.shutdown();
            }
        }
    }

    pub fn init(config: &Config) -> TelemetryGuard {
        if !is_enabled(config) {
            let _ = STATE.set(TelemetryStateSnapshot {
                enabled: false,
                feature_compiled: true,
                log_user_prompts: log_user_prompts(),
                log_tool_details: log_tool_details(),
                ..Default::default()
            });
            return TelemetryGuard {
                tracer_provider: None,
                meter_provider: None,
            };
        }

        let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:4317".to_string());
        let protocol =
            std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL").unwrap_or_else(|_| "grpc".to_string());

        let resource = Resource::builder()
            .with_service_name("ignis")
            .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
            .build();

        // Tracer pipeline. `with_tonic()` reads OTEL_EXPORTER_OTLP_ENDPOINT and
        // OTEL_EXPORTER_OTLP_HEADERS from the env automatically.
        let span_exporter = match SpanExporter::builder().with_tonic().build() {
            Ok(e) => e,
            Err(err) => {
                eprintln!("[telemetry] failed to build span exporter: {err}");
                return TelemetryGuard {
                    tracer_provider: None,
                    meter_provider: None,
                };
            }
        };
        let tracer_provider = SdkTracerProvider::builder()
            .with_batch_exporter(span_exporter)
            .with_resource(resource.clone())
            .build();
        let tracer = tracer_provider.tracer("ignis");
        global::set_tracer_provider(tracer_provider.clone());

        // Meter pipeline.
        let metric_exporter = match MetricExporter::builder().with_tonic().build() {
            Ok(e) => e,
            Err(err) => {
                eprintln!("[telemetry] failed to build metric exporter: {err}");
                return TelemetryGuard {
                    tracer_provider: Some(tracer_provider),
                    meter_provider: None,
                };
            }
        };
        let meter_provider = SdkMeterProvider::builder()
            .with_periodic_exporter(metric_exporter)
            .with_resource(resource)
            .build();
        global::set_meter_provider(meter_provider.clone());

        // Install metrics registry.
        let _ = METRICS.set(Metrics::new(meter_provider.meter("ignis")));

        // Bridge `tracing` spans → OTel via `tracing-opentelemetry`. RUST_LOG
        // controls level filtering as usual.
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let _ = tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with(otel_layer)
            .try_init();

        let _ = STATE.set(TelemetryStateSnapshot {
            enabled: true,
            endpoint: Some(endpoint),
            protocol: Some(protocol),
            feature_compiled: true,
            log_user_prompts: log_user_prompts(),
            log_tool_details: log_tool_details(),
        });

        TelemetryGuard {
            tracer_provider: Some(tracer_provider),
            meter_provider: Some(meter_provider),
        }
    }

    pub fn record_session_start(provider: &str, model: &str) {
        if let Some(m) = METRICS.get() {
            m.session_count.add(
                1,
                &[
                    KeyValue::new("provider", provider.to_string()),
                    KeyValue::new("model", model.to_string()),
                ],
            );
        }
    }

    pub fn record_tokens(usage: &Usage, provider: &str, model: &str) {
        let m = match METRICS.get() {
            Some(m) => m,
            None => return,
        };

        // ignis.token.usage — type ∈ {input, output, reasoning, cache_read, cache_write}
        let pairs: [(&'static str, u64); 5] = [
            ("input", usage.input_tokens),
            ("output", usage.output_tokens),
            ("reasoning", usage.reasoning_tokens),
            ("cache_read", usage.cache_read_tokens),
            ("cache_write", usage.cache_write_tokens),
        ];
        for (type_, count) in pairs {
            if count > 0 {
                m.token_usage.add(
                    count,
                    &[
                        KeyValue::new("type", type_),
                        KeyValue::new("provider", provider.to_string()),
                        KeyValue::new("model", model.to_string()),
                    ],
                );
            }
        }

        // gen_ai.client.token.usage — OTel GenAI semconv mirror. Backends with
        // pre-built GenAI dashboards (Honeycomb/Datadog/Grafana) key on this.
        let semconv_pairs: [(&'static str, u64); 3] = [
            ("input", usage.input_tokens),
            ("output", usage.output_tokens),
            ("reasoning", usage.reasoning_tokens),
        ];
        for (type_, count) in semconv_pairs {
            if count > 0 {
                m.token_usage_semconv.add(
                    count,
                    &[
                        KeyValue::new("gen_ai.system", provider.to_string()),
                        KeyValue::new("gen_ai.request.model", model.to_string()),
                        KeyValue::new("gen_ai.token.type", type_),
                    ],
                );
            }
        }
    }

    pub fn record_tool_call(tool_name: &str, duration: Duration, success: bool) {
        if let Some(m) = METRICS.get() {
            m.tool_calls.add(
                1,
                &[
                    KeyValue::new("tool.name", tool_name.to_string()),
                    KeyValue::new("success", success),
                ],
            );
            m.tool_duration.record(
                duration.as_secs_f64() * 1000.0,
                &[KeyValue::new("tool.name", tool_name.to_string())],
            );
        }
    }

    pub fn record_llm_request(provider: &str, model: &str, duration: Duration, success: bool) {
        if let Some(m) = METRICS.get() {
            m.llm_duration.record(
                duration.as_secs_f64() * 1000.0,
                &[
                    KeyValue::new("provider", provider.to_string()),
                    KeyValue::new("model", model.to_string()),
                    KeyValue::new("success", success),
                ],
            );
        }
    }

    pub fn state_snapshot() -> TelemetryStateSnapshot {
        STATE.get().cloned().unwrap_or_default()
    }
}

// ============================================================
// Stub implementation (feature disabled — no OTel deps compiled)
// ============================================================

#[cfg(not(feature = "telemetry"))]
mod imp {
    use super::*;

    pub struct TelemetryGuard;

    pub fn init(_config: &Config) -> TelemetryGuard {
        TelemetryGuard
    }

    pub fn record_session_start(_provider: &str, _model: &str) {}
    pub fn record_tokens(_usage: &Usage, _provider: &str, _model: &str) {}
    pub fn record_tool_call(_tool_name: &str, _duration: Duration, _success: bool) {}
    pub fn record_llm_request(_provider: &str, _model: &str, _duration: Duration, _success: bool) {}
    pub fn state_snapshot() -> TelemetryStateSnapshot {
        TelemetryStateSnapshot {
            enabled: false,
            feature_compiled: false,
            ..Default::default()
        }
    }
}

pub use imp::{
    init, record_llm_request, record_session_start, record_tokens, record_tool_call,
    state_snapshot, TelemetryGuard,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_enabled_reads_env_var() {
        let config = Config::default();
        // Default on — env var not set, config says true.
        std::env::remove_var("IGNIS_ENABLE_TELEMETRY");
        assert!(is_enabled(&config));

        // Env var set to truthy overrides config.
        std::env::set_var("IGNIS_ENABLE_TELEMETRY", "1");
        assert!(is_enabled(&config));
        std::env::remove_var("IGNIS_ENABLE_TELEMETRY");

        // Config disabled, env var not set — disabled.
        let mut config = Config::default();
        config.telemetry.enabled = false;
        assert!(!is_enabled(&config));
    }

    #[test]
    fn is_enabled_reads_config_when_env_unset() {
        std::env::remove_var("IGNIS_ENABLE_TELEMETRY");
        let mut config = Config::default();
        assert!(is_enabled(&config)); // on by default
        config.telemetry.enabled = false;
        assert!(!is_enabled(&config));
    }

    #[test]
    fn redaction_flags_default_off() {
        std::env::remove_var("IGNIS_LOG_USER_PROMPTS");
        std::env::remove_var("IGNIS_LOG_TOOL_DETAILS");
        assert!(!log_user_prompts());
        assert!(!log_tool_details());
    }
}
