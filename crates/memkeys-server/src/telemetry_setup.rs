//! OTel SDK wiring (M4): resource identity, exporters, sampling, and the
//! logs bridge — per the WS-7 contract sources.
//!
//! Contract sources (WS-7, owner Observability Agent):
//! - docs/observability/signal-catalog.yaml — resource attributes, sampling
//! - docs/adr/ADR-0001-observability-otel-signals.md — D-2 resource, D-5 sampling
//! - docs/observability/semconv-pins.yaml   — pinned release 1.30.0 (schema_url)
//!
//! Configuration is env/profile driven:
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` — gate: set → OTLP gRPC exporters for all
//!   three signals; unset → no-op providers (nothing is emitted off-box).
//!   The per-signal `OTEL_EXPORTER_OTLP_*` knobs (timeout, headers, protocol)
//!   are honored natively by the opentelemetry-otlp builders.
//! - `MEMKEYS_DEPLOYMENT_ENV` — deployment profile (dev|staging|prod) → the
//!   `deployment.environment.name` resource attribute AND the D-5 head
//!   sampling ratio (dev/staging 1.0; prod 0.1).
//! - `MEMKEYS_TRACES_SAMPLER_RATIO` — overrides the prod head-sampling ratio.
//! - `MEMKEYS_SERVICE_NAMESPACE` — optional `service.namespace`.
//!
//! Logs ride the tracing → OTel bridge (opentelemetry-appender-tracing): the
//! catalog's "OTel Logs SDK / log bridge" transport. Records emitted inside a
//! span auto-carry its TraceId/SpanId; bodies are templated event names only
//! (deny-list: no fact bodies, key values, or query text).

use std::time::Duration;

use opentelemetry::global;
use opentelemetry::KeyValue;
use opentelemetry_otlp::LogExporter;
use opentelemetry_otlp::MetricExporter;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::resource::TelemetryResourceDetector;
use opentelemetry_sdk::trace::Sampler;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;

use crate::telemetry::SCHEMA_URL;

/// The exporter gate (see module docs).
pub const OTEL_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
/// Deployment profile selector (`dev`|`staging`|`prod`).
pub const DEPLOYMENT_ENV_VAR: &str = "MEMKEYS_DEPLOYMENT_ENV";
/// Optional prod head-sampling ratio override (D-5: default 0.1).
pub const SAMPLER_RATIO_VAR: &str = "MEMKEYS_TRACES_SAMPLER_RATIO";
/// Optional `service.namespace` (ADR-0001 D-2: deployment grouping).
pub const NAMESPACE_VAR: &str = "MEMKEYS_SERVICE_NAMESPACE";

/// The fixed `service.name` (signal-catalog resource.required).
pub const SERVICE_NAME: &str = "memkeys";

/// Default prod head-sampling ratio (ADR-0001 D-5).
pub const PROD_SAMPLER_RATIO_DEFAULT: f64 = 0.1;

/// Deployment profile — the `deployment.environment.name` resource attribute
/// and the D-5 sampling driver (signal-catalog resource.required).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentProfile {
    Dev,
    Staging,
    Prod,
}

impl DeploymentProfile {
    /// Read `MEMKEYS_DEPLOYMENT_ENV`; defaults to `dev` (unset/empty). Any
    /// other value is a configuration error — the profile drives sampling,
    /// so a typo must not silently masquerade as dev.
    pub fn from_env() -> Result<Self, String> {
        let raw = std::env::var(DEPLOYMENT_ENV_VAR)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "dev".to_string());
        match raw.trim().to_ascii_lowercase().as_str() {
            "dev" => Ok(DeploymentProfile::Dev),
            "staging" => Ok(DeploymentProfile::Staging),
            "prod" => Ok(DeploymentProfile::Prod),
            other => Err(format!(
                "{DEPLOYMENT_ENV_VAR} must be one of dev|staging|prod, got {other:?}"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DeploymentProfile::Dev => "dev",
            DeploymentProfile::Staging => "staging",
            DeploymentProfile::Prod => "prod",
        }
    }

    /// D-5 head sampling: `ParentBased(TraceIdRatioBased(r))` with r from the
    /// profile — dev/staging keep everything, prod samples a configured
    /// fraction (default 0.1). Metrics and logs are never sampled.
    pub fn head_sampling_ratio(self) -> f64 {
        match self {
            DeploymentProfile::Dev | DeploymentProfile::Staging => 1.0,
            DeploymentProfile::Prod => std::env::var(SAMPLER_RATIO_VAR)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .map(|r| r.clamp(0.0, 1.0))
                .unwrap_or(PROD_SAMPLER_RATIO_DEFAULT),
        }
    }
}

/// `service.version` — semver + git sha per ADR-0001 D-2 ("build metadata").
/// The sha is embedded at build time by build.rs (falls back to `unknown`
/// outside a git checkout, e.g. a source tarball).
fn service_version() -> String {
    let sha = option_env!("MEMKEYS_BUILD_GIT_SHA").unwrap_or("unknown");
    format!("{}+{}", env!("CARGO_PKG_VERSION"), sha)
}

/// The process Resource (ADR-0001 D-2): identity set once, attached to every
/// signal, under the pinned schema_url. Tenant is NOT a resource attribute —
/// one process serves many tenants (D-4).
pub fn build_resource(profile: DeploymentProfile) -> Resource {
    let mut attrs = vec![
        KeyValue::new("service.name", SERVICE_NAME),
        KeyValue::new("service.version", service_version()),
        // Stable per-process UUID (replica disambiguation).
        KeyValue::new("service.instance.id", uuid::Uuid::new_v4().to_string()),
        KeyValue::new("deployment.environment.name", profile.as_str()),
    ];
    if let Ok(ns) = std::env::var(NAMESPACE_VAR) {
        if !ns.trim().is_empty() {
            attrs.push(KeyValue::new("service.namespace", ns));
        }
    }
    // builder_empty, not builder(): the SDK-provided default resource carries
    // service.name=unknown_service, and with_schema_url's merge order lets an
    // existing attribute win over the new ones — the catalog's identity set
    // (service.name=memkeys, pinned schema_url) must not be overridable by it.
    // builder_empty also drops the SDK detectors, including the one carrying
    // the OTel-spec MUST telemetry.sdk.{name,language,version} triple
    // (ADR-0001 D-2) — re-attached below, sourced from the SDK itself so
    // telemetry.sdk.version always tracks the opentelemetry_sdk crate. Its
    // keys don't overlap the identity set, and the detector merge keeps this
    // resource's schema_url.
    Resource::builder_empty()
        .with_schema_url(attrs, SCHEMA_URL)
        .with_detector(Box::new(TelemetryResourceDetector))
        .build()
}

/// Whether OTLP export is on for this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportMode {
    /// OTLP gRPC to `OTEL_EXPORTER_OTLP_ENDPOINT`.
    Otlp,
    /// No endpoint configured — global providers stay no-op.
    Noop,
}

/// The initialized providers. Hold this for the process lifetime; `Drop`
/// flushes and shuts everything down, so nothing is lost on exit.
#[derive(Debug)]
pub struct Otel {
    mode: ExportMode,
    profile: DeploymentProfile,
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl Otel {
    pub fn mode(&self) -> ExportMode {
        self.mode
    }

    pub fn profile(&self) -> DeploymentProfile {
        self.profile
    }

    /// Wire the global OTel providers from env/profile config.
    ///
    /// With `OTEL_EXPORTER_OTLP_ENDPOINT` set, all three signals ship over
    /// OTLP gRPC to a collector; without it, providers stay no-op (the M1
    /// library instrumentation is a no-op emission path). Errors are hard
    /// configuration failures (invalid profile, unusable endpoint) — the
    /// caller decides whether to fail startup.
    pub fn init() -> Result<Self, String> {
        let profile = DeploymentProfile::from_env()?;
        let resource = build_resource(profile);

        let endpoint = std::env::var(OTEL_ENDPOINT_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        match endpoint {
            None => {
                install_tracing_subscriber(None);
                Ok(Otel {
                    mode: ExportMode::Noop,
                    profile,
                    tracer_provider: None,
                    meter_provider: None,
                    logger_provider: None,
                })
            }
            Some(endpoint) => {
                let tracer_provider = build_tracer_provider(
                    &endpoint,
                    resource.clone(),
                    profile.head_sampling_ratio(),
                )?;
                let meter_provider = build_meter_provider(&endpoint, resource.clone())?;
                let logger_provider = build_logger_provider(&endpoint, resource.clone())?;

                global::set_tracer_provider(tracer_provider.clone());
                global::set_meter_provider(meter_provider.clone());
                install_tracing_subscriber(Some(&logger_provider));

                Ok(Otel {
                    mode: ExportMode::Otlp,
                    profile,
                    tracer_provider: Some(tracer_provider),
                    meter_provider: Some(meter_provider),
                    logger_provider: Some(logger_provider),
                })
            }
        }
    }
}

impl Drop for Otel {
    fn drop(&mut self) {
        // Flush then shut down in the order the signals were started. Metrics
        // and logs are never sampled, so a flush ships everything pending.
        if let Some(p) = self.logger_provider.take() {
            let _ = p.force_flush();
            let _ = p.shutdown();
        }
        if let Some(p) = self.meter_provider.take() {
            let _ = p.force_flush();
            let _ = p.shutdown();
        }
        if let Some(p) = self.tracer_provider.take() {
            let _ = p.force_flush();
            let _ = p.shutdown();
        }
    }
}

fn build_tracer_provider(
    endpoint: &str,
    resource: Resource,
    head_ratio: f64,
) -> Result<SdkTracerProvider, String> {
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_protocol(opentelemetry_otlp::Protocol::Grpc)
        .build()
        .map_err(|e| format!("OTLP span exporter init failed: {e}"))?;
    Ok(SdkTracerProvider::builder()
        // D-5 head sampling: parent-based so a sampled write keeps its
        // stage/worker children coherent. Metrics/logs are never sampled.
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            head_ratio,
        ))))
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build())
}

fn build_meter_provider(endpoint: &str, resource: Resource) -> Result<SdkMeterProvider, String> {
    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| format!("OTLP metric exporter init failed: {e}"))?;
    let reader = PeriodicReader::builder(exporter)
        .with_interval(Duration::from_secs(30))
        .build();
    Ok(SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(reader)
        .build())
}

fn build_logger_provider(endpoint: &str, resource: Resource) -> Result<SdkLoggerProvider, String> {
    let exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| format!("OTLP log exporter init failed: {e}"))?;
    Ok(SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build())
}

/// Install the global tracing subscriber: the OTel logs bridge (when enabled)
/// under a stdout fmt layer. The bridge is the catalog's log transport; the
/// fmt layer is a local console view, never the prod pipeline.
fn install_tracing_subscriber(logger_provider: Option<&SdkLoggerProvider>) {
    use tracing_subscriber::layer::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    match logger_provider {
        Some(provider) => {
            let bridge =
                opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(provider);
            // Only memkeys events ride the OTel log channel. This also breaks
            // a feedback loop: the SDK's own internal logs (e.g. a stopped
            // batch processor announcing itself) would otherwise re-enter
            // the same processor and recurse.
            let bridge = bridge.with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("memkeys", tracing_subscriber::filter::LevelFilter::TRACE),
            );
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(bridge)
                .with(tracing_subscriber::fmt::layer())
                .try_init();
        }
        None => {
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(tracing_subscriber::fmt::layer())
                .try_init();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_defaults_to_dev_and_rejects_garbage() {
        std::env::remove_var(DEPLOYMENT_ENV_VAR);
        assert_eq!(DeploymentProfile::from_env(), Ok(DeploymentProfile::Dev));
        // SAFETY: tests run multi-threaded; this env is only read in tests
        // that hold the otel lock or are profile-only, and this one writes a
        // value no other test reads.
        std::env::set_var(DEPLOYMENT_ENV_VAR, "prod");
        assert_eq!(DeploymentProfile::from_env(), Ok(DeploymentProfile::Prod));
        assert_eq!(
            DeploymentProfile::from_env().unwrap().head_sampling_ratio(),
            PROD_SAMPLER_RATIO_DEFAULT
        );
        std::env::set_var(SAMPLER_RATIO_VAR, "0.5");
        assert_eq!(
            DeploymentProfile::from_env().unwrap().head_sampling_ratio(),
            0.5
        );
        std::env::remove_var(SAMPLER_RATIO_VAR);
        std::env::set_var(DEPLOYMENT_ENV_VAR, "production");
        assert!(DeploymentProfile::from_env().is_err());
        std::env::remove_var(DEPLOYMENT_ENV_VAR);
    }

    #[test]
    fn resource_carries_catalog_identity_and_pinned_schema() {
        let resource = build_resource(DeploymentProfile::Dev);
        let get = |k: &str| resource.get(&opentelemetry::Key::new(k.to_string()));
        assert_eq!(
            get("service.name").map(|v| v.to_string()),
            Some("memkeys".to_string())
        );
        assert_eq!(
            get("deployment.environment.name").map(|v| v.to_string()),
            Some("dev".to_string())
        );
        assert!(get("service.instance.id").is_some());
        assert!(get("service.version").is_some());
        // OTel spec MUST (ADR-0001 D-2): builder_empty drops the SDK's
        // telemetry.sdk.* detector, so the triple is re-attached explicitly.
        assert_eq!(
            get("telemetry.sdk.name").map(|v| v.to_string()),
            Some("opentelemetry".to_string())
        );
        assert_eq!(
            get("telemetry.sdk.language").map(|v| v.to_string()),
            Some("rust".to_string())
        );
        // The detector reports the opentelemetry_sdk crate version.
        let sdk_version = get("telemetry.sdk.version").map(|v| v.to_string());
        assert!(
            sdk_version
                .as_deref()
                .map(|v| !v.is_empty())
                .unwrap_or(false),
            "telemetry.sdk.version must be a non-empty SDK version, got {sdk_version:?}"
        );
        assert_eq!(resource.schema_url(), Some(SCHEMA_URL));
        // D-2/D-4: tenant is NEVER a resource attribute.
        assert!(get("memkeys.tenant.id").is_none());
    }
}
