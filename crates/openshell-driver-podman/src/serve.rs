// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The launched-driver commands: `serve` and `preflight`.
//!
//! The gateway starts `serve` with bootstrap arguments only and the operator's
//! `[openshell.drivers.podman]` table on stdin, then delivers gateway-owned
//! metadata with the `Configure` RPC. Until `Configure` succeeds the driver
//! creates no Podman networks, volumes or secrets.

use std::path::PathBuf;

use clap::Parser;
use openshell_core::VERSION;
use openshell_core::proto::compute::v1::compute_driver_server::ComputeDriverServer;
use openshell_driver_podman::config::settings_from_table;
use openshell_driver_podman::driver::check_backend;
use openshell_driver_podman::{ComputeDriverService, PodmanComputeConfig, PodmanComputeDriver};
use openshell_driver_sdk::launch::{ConfigureRequest, Deferred, Launch, LaunchInfo};
use openshell_driver_sdk::{GatewayContext, StartResult};
use tonic::Status;
use tracing::info;

/// Bootstrap arguments the gateway passes to `serve`. They are frozen for
/// protocol version 1.
#[derive(Parser, Debug)]
#[command(name = "openshell-driver-podman serve")]
struct ServeArgs {
    #[arg(long)]
    bind_socket: PathBuf,

    #[arg(long)]
    expected_peer_pid: u32,

    #[arg(long, default_value = "info")]
    log_level: String,
}

/// Run `serve` or `preflight` when that is the first argument and return the
/// process exit code. Any other invocation returns `None` and takes the
/// standalone path.
pub async fn dispatch() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1)?.as_str() {
        "serve" => {
            let parsed = ServeArgs::parse_from(
                std::iter::once("openshell-driver-podman serve".to_string())
                    .chain(args.into_iter().skip(2)),
            );
            Some(serve(parsed).await)
        }
        "preflight" => Some(preflight()),
        _ => None,
    }
}

fn read_settings() -> Result<PodmanComputeConfig, StartResult> {
    let table = openshell_driver_sdk::read_stdin_table()
        .map_err(|error| StartResult::invalid_config(format!("read driver table: {error}")))?;
    let mut config = settings_from_table(&table).map_err(StartResult::invalid_config)?;
    // The environment variable wins over the table, as it always has.
    if let Ok(path) = std::env::var("OPENSHELL_PODMAN_SOCKET") {
        config.socket_path = Some(path.into());
    }
    Ok(config)
}

fn preflight() -> i32 {
    match read_settings() {
        Ok(_) => 0,
        Err(result) => result.report(),
    }
}

async fn serve(args: ServeArgs) -> i32 {
    let tracing = openshell_otel::install_deferred_driver_tracing(
        openshell_driver_podman::otel_tracing::TRACING,
        &args.log_level,
        openshell_core::color::stdout_ansi(),
    );
    match run(args, tracing).await {
        Ok(()) => 0,
        Err(result) => result.report(),
    }
}

async fn run(
    args: ServeArgs,
    tracing: openshell_otel::DeferredDriverTracing,
) -> Result<(), StartResult> {
    let settings = read_settings()?;
    openshell_driver_sdk::check_backend_within("Podman", check_backend(&settings)).await?;

    let listener = openshell_core::external_driver_socket::bind_private(&args.bind_socket)
        .map_err(StartResult::error)?;
    let _cleanup =
        openshell_core::external_driver_socket::SocketCleanup::new(args.bind_socket.clone());

    let launch = Launch::new(
        LaunchInfo {
            driver_name: "podman".to_string(),
            driver_version: VERSION.to_string(),
            features: Vec::new(),
        },
        move |request| {
            let settings = settings.clone();
            let tracing = tracing.clone();
            async move { apply_configure(settings, &tracing, request).await }
        },
    );
    let shutdown = launch.shutdown();
    #[cfg(unix)]
    openshell_driver_sdk::watch_parent(args.expected_peer_pid, shutdown.clone());
    let deferred = Deferred::new(launch.configured(), |service: &ComputeDriverService| {
        ComputeDriverServer::new(service.clone())
    });

    info!(socket = %args.bind_socket.display(), "Starting Podman compute driver");
    tonic::transport::Server::builder()
        .layer(openshell_otel::compute_driver_rpc_layer())
        .add_service(launch.into_server())
        .add_service(deferred)
        .serve_with_incoming_shutdown(
            openshell_core::external_driver_socket::SameUidUnixIncoming::new(listener)
                .expecting_peer_pid(args.expected_peer_pid),
            openshell_driver_sdk::stop_when_released(
                shutdown,
                crate::shutdown_signal(),
                openshell_driver_sdk::EXTERNAL_STOP_GRACE,
            ),
        )
        .await
        .map_err(|error| StartResult::error(format!("serve: {error}")))
}

/// Build the driver from the operator's settings and the gateway's metadata.
async fn apply_configure(
    settings: PodmanComputeConfig,
    tracing: &openshell_otel::DeferredDriverTracing,
    request: ConfigureRequest,
) -> Result<ComputeDriverService, Status> {
    let gateway = GatewayContext::try_from(&request.gateway.clone().unwrap_or_default())?;
    if let Some(export) = openshell_driver_sdk::deferred_export_config(
        request.telemetry.as_ref(),
        gateway.gateway_name.as_deref(),
        VERSION,
    )? && let Err(error) = tracing.enable_export(export)
    {
        tracing::error!(%error, "OTLP exporting could not be started");
    }
    let supervisor_otlp = openshell_driver_sdk::supervisor_otlp(request.telemetry.as_ref())?;
    let network_trust = openshell_driver_sdk::load_network_trust(request.network_trust.as_ref())?;
    let admission =
        openshell_driver_sdk::admission_from_json(&request.resource_admission_policy_json)?;
    let mut config = driver_config(
        settings,
        &gateway,
        request.image_overrides.as_ref(),
        supervisor_otlp,
    )?;
    config.allow_driver_config = admission.allow_driver_config;
    config.resource_admission = admission.resource_admission;
    let driver = PodmanComputeDriver::new(config, network_trust)
        .await
        .map_err(|error| Status::internal(error.to_string()))?;
    Ok(ComputeDriverService::new(driver))
}

/// The driver configuration for `settings` and the gateway's metadata. The
/// gateway supplies the port and the CA, which the table cannot set. The
/// image overrides win, as the environment did when the gateway built the
/// configuration itself.
fn driver_config(
    mut settings: PodmanComputeConfig,
    gateway: &GatewayContext,
    images: Option<&openshell_driver_sdk::launch::ImageOverrides>,
    supervisor_otlp: openshell_core::sandbox_env::SupervisorOtlp,
) -> Result<PodmanComputeConfig, Status> {
    if let Some(port) = gateway.gateway_port {
        settings.gateway_port = port;
    }
    settings.guest_tls_ca.clone_from(&gateway.tls_ca_path);
    if openshell_driver_sdk::endpoint_uses_tls(&settings.grpc_endpoint)
        && settings.guest_tls_ca.is_none()
    {
        return Err(Status::invalid_argument(
            "grpc_endpoint uses https:// but the gateway supplied no CA path",
        ));
    }
    if let Some(images) = images {
        if let Some(image) = &images.sandbox_runtime_image {
            settings.sandbox_runtime_image.clone_from(image);
        }
        if let Some(image) = &images.supervisor_image {
            settings.supervisor_image.clone_from(image);
        }
    }
    settings.supervisor_otlp = supervisor_otlp;
    settings
        .supervisor_gateway_name
        .clone_from(&gateway.gateway_name);
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use openshell_driver_sdk::launch::ImageOverrides;

    use super::*;

    fn gateway(port: Option<u16>, ca: Option<&str>) -> GatewayContext {
        GatewayContext {
            gateway_port: port,
            tls_ca_path: ca.map(PathBuf::from),
            gateway_name: Some("gw".to_string()),
            bind_address: None,
        }
    }

    fn build(
        settings: PodmanComputeConfig,
        gateway: &GatewayContext,
        images: Option<&ImageOverrides>,
    ) -> Result<PodmanComputeConfig, Status> {
        driver_config(
            settings,
            gateway,
            images,
            openshell_core::sandbox_env::SupervisorOtlp::default(),
        )
    }

    #[test]
    fn the_gateway_supplies_the_port_name_and_ca() {
        let built = build(
            PodmanComputeConfig::default(),
            &gateway(Some(9000), Some("/gateway-ca.pem")),
            None,
        )
        .unwrap();
        assert_eq!(built.gateway_port, 9000);
        assert_eq!(built.supervisor_gateway_name.as_deref(), Some("gw"));
        assert_eq!(built.guest_tls_ca, Some("/gateway-ca.pem".into()));

        let plain = build(PodmanComputeConfig::default(), &gateway(None, None), None).unwrap();
        assert!(plain.guest_tls_ca.is_none());
    }

    #[test]
    fn an_https_endpoint_needs_the_gateway_ca() {
        let table = settings_from_table("grpc_endpoint = \"https://host.test:9\"").unwrap();
        let status = build(table.clone(), &gateway(Some(1), None), None).unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        build(table, &gateway(Some(1), Some("/ca.pem")), None).unwrap();
    }

    #[test]
    fn image_overrides_win_over_the_table_as_the_environment_did() {
        let table = settings_from_table(
            "sandbox_runtime_image = \"table-runtime\"\nsupervisor_image = \"table-supervisor\"",
        )
        .unwrap();
        let overrides = ImageOverrides {
            sandbox_runtime_image: Some("env-runtime".to_string()),
            supervisor_image: None,
        };
        let built = build(table, &gateway(None, None), Some(&overrides)).unwrap();
        assert_eq!(built.sandbox_runtime_image, "env-runtime");
        assert_eq!(built.supervisor_image, "table-supervisor");
    }
}
