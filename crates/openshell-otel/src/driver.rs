// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared compute-driver tracing setup and in-process RPC instrumentation.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use futures::Stream;
use tracing::Instrument as _;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

use crate::{OtlpExportConfig, SdkTracerProvider, ServiceName, SetupError};

/// Target used by every in-process compute-driver RPC boundary.
pub const IN_PROCESS_COMPUTE_DRIVER_TARGET: &str = "openshell_otel::in_process_compute_driver";

/// Define the calling driver crate's [`ComputeDriverTracing`] identity from
/// its Cargo package and crate names.
#[macro_export]
macro_rules! compute_driver_tracing {
    () => {
        $crate::ComputeDriverTracing::new(env!("CARGO_PKG_NAME"), env!("CARGO_CRATE_NAME"))
    };
}

/// Tracing identity and layer factory for one compute-driver service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputeDriverTracing {
    service_name: &'static str,
    crate_target_prefix: &'static str,
}

impl ComputeDriverTracing {
    /// `crate_target_prefix` must be the driver's Rust crate name; it routes
    /// that crate's spans to the driver's provider. Prefer
    /// [`compute_driver_tracing!`](crate::compute_driver_tracing).
    #[must_use]
    pub const fn new(service_name: &'static str, crate_target_prefix: &'static str) -> Self {
        Self {
            service_name,
            crate_target_prefix,
        }
    }

    #[must_use]
    pub const fn service_name(self) -> &'static str {
        self.service_name
    }

    /// Canonical compute-driver name derived from `service.name`.
    #[must_use]
    pub fn compute_driver(self) -> &'static str {
        self.service_name
            .strip_prefix("openshell-driver-")
            .unwrap_or(self.service_name)
    }

    #[must_use]
    pub const fn in_process_target(self) -> &'static str {
        IN_PROCESS_COMPUTE_DRIVER_TARGET
    }

    /// Targets routed to this driver's provider when it runs in-process.
    #[must_use]
    pub const fn in_process_targets(self) -> [&'static str; 2] {
        [self.in_process_target(), self.crate_target_prefix]
    }

    #[must_use]
    pub fn provider_for(
        self,
        endpoint: Option<&str>,
        transport: &crate::CollectorTransport,
        service_version: &'static str,
        gateway_name: Option<&str>,
        compute_driver: Option<&str>,
    ) -> (Option<SdkTracerProvider>, Option<SetupError>) {
        let provider = endpoint.map(|endpoint| {
            crate::OtlpTarget::new(&OtlpExportConfig {
                endpoint,
                service_name: ServiceName::Fixed(self.service_name),
                service_version: Some(service_version),
                resource_attributes: crate::gateway_resource_attributes(
                    gateway_name,
                    compute_driver,
                ),
            })?
            .with_transport(transport)
            .tracer_provider()
        });
        match provider {
            None => (None, None),
            Some(Ok(provider)) => (Some(provider), None),
            Some(Err(error)) => (None, Some(error)),
        }
    }

    pub fn layer<S>(self, provider: &SdkTracerProvider) -> crate::OtlpLayer<S>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span>,
    {
        crate::layer(provider, self.service_name)
    }

    pub fn in_process_layer<S>(self, provider: &SdkTracerProvider) -> crate::TargetOtlpLayer<S>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span>,
    {
        crate::layer_for_target_prefixes(provider, self.service_name, self.in_process_targets())
    }
}

/// Optional in-process server boundary tracing for a compute-driver service.
#[derive(Debug, Clone, Copy)]
pub struct InProcessRpcTracer {
    enabled: bool,
}

impl InProcessRpcTracer {
    #[must_use]
    pub const fn disabled() -> Self {
        Self { enabled: false }
    }

    #[must_use]
    pub const fn enabled() -> Self {
        Self { enabled: true }
    }

    fn span(self, rpc: crate::ComputeDriverRpc) -> Option<tracing::Span> {
        self.enabled.then(|| {
            tracing::info_span!(
                target: IN_PROCESS_COMPUTE_DRIVER_TARGET,
                "driver_rpc",
                otel.name = rpc.operation,
                otel.kind = "server",
                otel.status_code = tracing::field::Empty,
                rpc.system.name = "grpc",
                rpc.method = rpc.operation,
                rpc.response.status_code = tracing::field::Empty,
                error.type = tracing::field::Empty,
            )
        })
    }

    pub async fn trace<T>(
        self,
        rpc: crate::ComputeDriverRpc,
        future: impl Future<Output = Result<T, tonic::Status>>,
    ) -> Result<T, tonic::Status> {
        let Some(span) = self.span(rpc) else {
            return future.await;
        };
        let result = future.instrument(span.clone()).await;
        record_result(&span, &result);
        result
    }

    pub async fn trace_stream<T>(
        self,
        rpc: crate::ComputeDriverRpc,
        future: impl Future<Output = Result<BoxGrpcStream<T>, tonic::Status>>,
    ) -> Result<BoxGrpcStream<T>, tonic::Status>
    where
        T: 'static,
    {
        let Some(span) = self.span(rpc) else {
            return future.await;
        };
        match future.instrument(span.clone()).await {
            Ok(stream) => Ok(Box::pin(crate::TracedGrpcStream::new(stream, span))),
            Err(status) => {
                crate::record_grpc_status(&span, status.code());
                Err(status)
            }
        }
    }
}

pub type BoxGrpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, tonic::Status>> + Send + 'static>>;

fn record_result<T>(span: &tracing::Span, result: &Result<T, tonic::Status>) {
    crate::record_grpc_status(
        span,
        result
            .as_ref()
            .map_or_else(tonic::Status::code, |_| tonic::Code::Ok),
    );
}

/// Owns and shuts down a standalone compute driver's tracer provider.
pub struct DriverTracingHandle {
    provider: Option<SdkTracerProvider>,
}

/// Named inputs for standalone compute-driver tracing installation.
#[derive(Debug, Clone, Copy)]
pub struct DriverTracingConfig<'a> {
    pub endpoint: Option<&'a str>,
    pub protocol: crate::Protocol,
    /// PEM files for an `https://` collector; see [`crate::OtlpTls::from_files`].
    pub ca_file: Option<&'a std::path::Path>,
    pub cert_file: Option<&'a std::path::Path>,
    pub key_file: Option<&'a std::path::Path>,
    pub gateway_name: Option<&'a str>,
    pub service_version: &'static str,
    pub log_level: &'a str,
    /// Whether console logs on stdout carry ANSI escapes. Callers pass
    /// `openshell_core::color::stdout_ansi()` so collected logs stay plain.
    pub ansi: bool,
}

impl Drop for DriverTracingHandle {
    fn drop(&mut self) {
        if let Some(provider) = &self.provider
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!(%error, "OTLP tracer provider shutdown failed");
        }
    }
}

impl DriverTracingConfig<'_> {
    /// The collector transport, with headers from the environment only.
    pub fn transport(&self) -> Result<crate::CollectorTransport, crate::TlsFileError> {
        Ok(crate::CollectorTransport {
            protocol: self.protocol,
            headers: http::HeaderMap::new(),
            tls: crate::OtlpTls::from_files(self.ca_file, self.cert_file, self.key_file)?,
        })
    }
}

/// Install standalone compute-driver logging and OTLP tracing.
#[must_use]
pub fn install_driver_tracing(
    descriptor: ComputeDriverTracing,
    config: DriverTracingConfig<'_>,
) -> DriverTracingHandle {
    let (provider, setup_error) = match config.transport() {
        Ok(transport) => descriptor.provider_for(
            config.endpoint,
            &transport,
            config.service_version,
            config.gateway_name,
            Some(descriptor.compute_driver()),
        ),
        Err(error) => (None, config.endpoint.map(|_| error.into())),
    };
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(config.log_level)),
        )
        .with(tracing_subscriber::fmt::layer().with_ansi(config.ansi))
        .with(provider.as_ref().map(|provider| descriptor.layer(provider)))
        .init();

    if let Some(error) = setup_error {
        tracing::error!(%error, "OTLP exporting could not be started");
    } else if let Some(endpoint) = config.endpoint {
        tracing::info!(endpoint, "OTLP exporting enabled");
    }

    DriverTracingHandle { provider }
}

type ExportLayer = tracing_opentelemetry::OpenTelemetryLayer<
    tracing_subscriber::Registry,
    opentelemetry_sdk::trace::Tracer,
>;

/// Collector settings supplied after a launched driver is already logging.
#[derive(Debug, Clone, Copy)]
pub struct DeferredExportConfig<'a> {
    pub endpoint: &'a str,
    pub protocol: crate::Protocol,
    /// PEM files for an `https://` collector; see [`crate::OtlpTls::from_files`].
    pub ca_file: Option<&'a std::path::Path>,
    pub cert_file: Option<&'a std::path::Path>,
    pub key_file: Option<&'a std::path::Path>,
    pub gateway_name: Option<&'a str>,
    pub service_version: &'static str,
    /// Collector request headers in `OTEL_EXPORTER_OTLP_HEADERS` format.
    pub headers: &'a str,
}

/// A tracing layer whose OpenTelemetry export is installed once, later.
///
/// Unlike `reload::Layer`, it forwards `downcast_raw` to the installed layer.
/// `OpenTelemetrySpanExt::set_parent` finds the OpenTelemetry layer through
/// that downcast, so a reloadable slot would silently drop the gateway's
/// parent trace context.
struct DeferredLayer {
    slot: Arc<OnceLock<ExportLayer>>,
}

impl tracing_subscriber::Layer<tracing_subscriber::Registry> for DeferredLayer {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>,
    ) {
        if let Some(layer) = self.slot.get() {
            layer.on_new_span(attrs, id, ctx);
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>,
    ) {
        if let Some(layer) = self.slot.get() {
            layer.on_record(id, values, ctx);
        }
    }

    fn on_follows_from(
        &self,
        id: &tracing::span::Id,
        follows: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>,
    ) {
        if let Some(layer) = self.slot.get() {
            layer.on_follows_from(id, follows, ctx);
        }
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        ctx: tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>,
    ) {
        if let Some(layer) = self.slot.get() {
            layer.on_event(event, ctx);
        }
    }

    fn on_enter(
        &self,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>,
    ) {
        if let Some(layer) = self.slot.get() {
            layer.on_enter(id, ctx);
        }
    }

    fn on_exit(
        &self,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>,
    ) {
        if let Some(layer) = self.slot.get() {
            layer.on_exit(id, ctx);
        }
    }

    fn on_close(
        &self,
        id: tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, tracing_subscriber::Registry>,
    ) {
        if let Some(layer) = self.slot.get() {
            layer.on_close(id, ctx);
        }
    }

    // `downcast_raw` is an unsafe trait method; this implementation only
    // returns pointers the installed layer itself returns, or to `self`.
    #[allow(unsafe_code)]
    unsafe fn downcast_raw(&self, id: std::any::TypeId) -> Option<*const ()> {
        if id == std::any::TypeId::of::<Self>() {
            return Some(std::ptr::from_ref(self).cast());
        }
        // SAFETY: the slot is set at most once, and the layer stays at the
        // same address in the `Arc` for as long as this layer exists.
        self.slot
            .get()
            .and_then(|layer| unsafe { layer.downcast_raw(id) })
    }
}

/// Handle to a launched driver's tracing, whose OTLP export starts later.
///
/// Cloning shares the same exporter slot. The provider shuts down when the
/// last clone drops.
#[derive(Clone)]
pub struct DeferredDriverTracing {
    inner: Arc<DeferredInner>,
}

struct DeferredInner {
    descriptor: ComputeDriverTracing,
    slot: Arc<OnceLock<ExportLayer>>,
    provider: Mutex<Option<SdkTracerProvider>>,
}

impl Drop for DeferredInner {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(provider) = provider
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!(%error, "OTLP tracer provider shutdown failed");
        }
    }
}

impl DeferredDriverTracing {
    /// Start exporting spans created from now on. Spans created earlier are
    /// not exported. Later calls are no-ops that return `Ok`, so a retried
    /// configuration never installs a second exporter.
    pub fn enable_export(&self, config: DeferredExportConfig<'_>) -> Result<(), SetupError> {
        use opentelemetry::trace::TracerProvider as _;

        let mut provider_slot = self
            .inner
            .provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if provider_slot.is_some() {
            return Ok(());
        }
        let transport = crate::CollectorTransport {
            protocol: config.protocol,
            headers: crate::parse_otlp_headers(config.headers),
            tls: crate::OtlpTls::from_files(config.ca_file, config.cert_file, config.key_file)?,
        };
        let descriptor = self.inner.descriptor;
        let (provider, error) = descriptor.provider_for(
            Some(config.endpoint),
            &transport,
            config.service_version,
            config.gateway_name,
            Some(descriptor.compute_driver()),
        );
        if let Some(error) = error {
            return Err(error);
        }
        let Some(provider) = provider else {
            return Ok(());
        };
        let layer =
            tracing_opentelemetry::layer().with_tracer(provider.tracer(descriptor.service_name()));
        if self.inner.slot.set(layer).is_err() {
            let _ = provider.shutdown();
            return Ok(());
        }
        tracing::info!(endpoint = config.endpoint, "OTLP exporting enabled");
        *provider_slot = Some(provider);
        Ok(())
    }
}

/// Install logging for a launched driver with an empty OTLP slot to fill
/// later with [`DeferredDriverTracing::enable_export`].
///
/// Logs go to stderr: a launched driver's stdout carries its start-up result.
#[must_use]
pub fn install_deferred_driver_tracing(
    descriptor: ComputeDriverTracing,
    log_level: &str,
    ansi: bool,
) -> DeferredDriverTracing {
    let slot = Arc::new(OnceLock::new());
    // The span filter stays on the deferred layer's wrapper, so it is
    // registered before the OpenTelemetry layer exists.
    tracing_subscriber::registry()
        .with(
            DeferredLayer {
                slot: Arc::clone(&slot),
            }
            .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                metadata.is_span() && !metadata.target().starts_with("opentelemetry")
            })),
        )
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level)))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(ansi)
                .with_writer(std::io::stderr),
        )
        .init();
    DeferredDriverTracing {
        inner: Arc::new(DeferredInner {
            descriptor,
            slot,
            provider: Mutex::new(None),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracing_macro_derives_identity_from_the_calling_crate() {
        const TRACING: ComputeDriverTracing = crate::compute_driver_tracing!();

        assert_eq!(TRACING.service_name(), "openshell-otel");
        assert_eq!(TRACING.in_process_targets()[1], "openshell_otel");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_provider_exports_with_the_configured_protocol_and_headers() {
        crate::test_environment::Environment::new()
            .remove("OTEL_EXPORTER_OTLP_HEADERS")
            .remove("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
            .remove("OTEL_EXPORTER_OTLP_LOGS_HEADERS")
            .run_async(async {
                use opentelemetry::trace::{Tracer as _, TracerProvider as _};

                const TRACING: ComputeDriverTracing = crate::compute_driver_tracing!();
                let mut collector = openshell_otel_test_support::OtlpHttpTestServer::start().await;
                let (provider, error) = TRACING.provider_for(
                    Some(collector.endpoint()),
                    &crate::CollectorTransport {
                        protocol: crate::Protocol::HttpProtobuf,
                        headers: crate::parse_otlp_headers("authorization=driver"),
                        tls: crate::OtlpTls::default(),
                    },
                    "1.2.3",
                    Some("test-gateway"),
                    Some("docker"),
                );
                assert!(error.is_none(), "unexpected setup error: {error:?}");
                let provider = provider.expect("provider");
                provider.tracer("test").in_span("driver-span", |_| {});
                let flush = tokio::task::spawn_blocking(move || provider.force_flush());

                let export = collector.next_export().await;
                assert_eq!(export.path, "/v1/traces");
                assert_eq!(export.headers["authorization"], "driver");
                flush.await.unwrap().expect("flush succeeds");
            })
            .await;
    }

    #[test]
    fn driver_transport_reads_its_tls_files() {
        let pki = openshell_otel_test_support::TestPki::generate();
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, &pki.ca_pem).unwrap();
        let config = |ca_file, key_file| DriverTracingConfig {
            endpoint: Some("https://collector:4317"),
            protocol: crate::Protocol::Grpc,
            ca_file,
            cert_file: None,
            key_file,
            gateway_name: None,
            service_version: "1.2.3",
            log_level: "info",
            ansi: false,
        };

        let transport = config(Some(&ca), None).transport().unwrap();
        assert_eq!(transport.tls.ca_pem(), Some(pki.ca_pem.as_bytes()));
        assert!(matches!(
            config(None, Some(&ca)).transport(),
            Err(crate::TlsFileError::KeyWithoutCert)
        ));
    }

    #[tokio::test]
    async fn disabled_stream_tracing_returns_the_original_box() {
        let stream: BoxGrpcStream<()> = Box::pin(futures::stream::empty());
        let original = std::ptr::from_ref(stream.as_ref().get_ref());

        let returned = InProcessRpcTracer::disabled()
            .trace_stream(crate::rpc::WATCH_SANDBOXES, async move { Ok(stream) })
            .await
            .unwrap();

        assert!(std::ptr::eq(original, returned.as_ref().get_ref()));
    }
}
