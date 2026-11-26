// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The launched-driver commands: `serve` and `preflight`.
//!
//! The gateway starts `serve` with bootstrap arguments only and the operator's
//! `[openshell.drivers.docker]` table on stdin, then delivers gateway-owned
//! metadata with the `Configure` RPC. Until `Configure` succeeds the driver
//! creates no Docker volumes, networks or containers, and both of its gRPC
//! services answer `UNAVAILABLE`.

use std::path::PathBuf;

use clap::Parser;
use openshell_core::VERSION;
use openshell_core::proto::compute::v1::compute_driver_configuration_updates_server::ComputeDriverConfigurationUpdatesServer;
use openshell_core::proto::compute::v1::compute_driver_server::ComputeDriverServer;
use openshell_driver_docker::launch::{check_backend, settings_from_table};
use openshell_driver_docker::{ComputeDriverService, DockerComputeConfig, DockerComputeDriver};
use openshell_driver_sdk::launch::{ConfigureRequest, Deferred, Launch, LaunchInfo};
use openshell_driver_sdk::{GatewayContext, StartResult};
use tonic::Status;
use tracing::info;

/// Bootstrap arguments the gateway passes to `serve`. They are frozen for
/// protocol version 1.
#[derive(Parser, Debug)]
#[command(name = "openshell-driver-docker serve")]
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
                std::iter::once("openshell-driver-docker serve".to_string())
                    .chain(args.into_iter().skip(2)),
            );
            Some(serve(parsed).await)
        }
        "preflight" => Some(preflight()),
        _ => None,
    }
}

fn read_settings() -> Result<DockerComputeConfig, StartResult> {
    let table = openshell_driver_sdk::read_stdin_table()
        .map_err(|error| StartResult::invalid_config(format!("read driver table: {error}")))?;
    settings_from_table(&table).map_err(StartResult::invalid_config)
}

fn preflight() -> i32 {
    match read_settings() {
        Ok(_) => 0,
        Err(result) => result.report(),
    }
}

async fn serve(args: ServeArgs) -> i32 {
    let tracing = openshell_otel::install_deferred_driver_tracing(
        openshell_driver_docker::otel_tracing::TRACING,
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
    openshell_driver_sdk::check_backend_within("Docker", check_backend(&settings)).await?;

    let listener = openshell_core::external_driver_socket::bind_private(&args.bind_socket)
        .map_err(StartResult::error)?;
    let _cleanup =
        openshell_core::external_driver_socket::SocketCleanup::new(args.bind_socket.clone());

    let log_level = args.log_level.clone();
    let launch = Launch::new(
        LaunchInfo {
            driver_name: "docker".to_string(),
            driver_version: VERSION.to_string(),
            features: Vec::new(),
        },
        move |request| {
            let settings = settings.clone();
            let tracing = tracing.clone();
            let log_level = log_level.clone();
            async move { apply_configure(settings, &log_level, &tracing, request).await }
        },
    );
    let shutdown = launch.shutdown();
    #[cfg(unix)]
    openshell_driver_sdk::watch_parent(args.expected_peer_pid, shutdown.clone());
    // One configured service backs both gRPC services.
    let compute = Deferred::new(launch.configured(), |service: &ComputeDriverService| {
        ComputeDriverServer::new(service.clone())
    });
    let configuration_updates =
        Deferred::new(launch.configured(), |service: &ComputeDriverService| {
            ComputeDriverConfigurationUpdatesServer::new(service.clone())
        });

    info!(socket = %args.bind_socket.display(), "Starting Docker compute driver");
    tonic::transport::Server::builder()
        .layer(openshell_otel::compute_driver_rpc_layer())
        .add_service(launch.into_server())
        .add_service(compute)
        .add_service(configuration_updates)
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
    settings: DockerComputeConfig,
    log_level: &str,
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
    let (config, bind_address) = driver_config(
        settings,
        &gateway,
        request.image_overrides.as_ref(),
        supervisor_otlp,
        &admission,
    )?;
    let driver = DockerComputeDriver::new(bind_address, log_level, &config, network_trust)
        .await
        .map_err(|error| Status::internal(error.to_string()))?;
    Ok(ComputeDriverService::new(driver))
}

/// The driver configuration for `settings` and the gateway's metadata, and
/// the gateway's listener. The gateway supplies the CA and the admission
/// policy, which the table cannot set. Image overrides win over the table, as
/// the environment did when the gateway built the configuration itself.
fn driver_config(
    mut settings: DockerComputeConfig,
    gateway: &GatewayContext,
    images: Option<&openshell_driver_sdk::launch::ImageOverrides>,
    supervisor_otlp: openshell_core::sandbox_env::SupervisorOtlp,
    admission: &openshell_core::resource_admission::DriverAdmissionConfig,
) -> Result<(DockerComputeConfig, std::net::SocketAddr), Status> {
    let bind_address = gateway
        .bind_address
        .ok_or_else(|| Status::invalid_argument("the gateway did not send its bind address"))?;
    settings.guest_tls_ca.clone_from(&gateway.tls_ca_path);
    if openshell_driver_sdk::endpoint_uses_tls(&settings.grpc_endpoint)
        && settings.guest_tls_ca.is_none()
    {
        return Err(Status::invalid_argument(
            "grpc_endpoint uses https:// but the gateway supplied no CA path",
        ));
    }
    settings.allow_driver_config = admission.allow_driver_config;
    settings.resource_admission = admission.resource_admission.clone();
    if let Some(images) = images {
        if let Some(image) = &images.sandbox_runtime_image {
            settings.sandbox_runtime_image = Some(image.clone());
        }
        if let Some(image) = &images.supervisor_image {
            settings.supervisor_image = Some(image.clone());
        }
    }
    settings.supervisor_otlp = supervisor_otlp;
    settings
        .supervisor_gateway_name
        .clone_from(&gateway.gateway_name);
    Ok((settings, bind_address))
}

#[cfg(test)]
mod tests {
    use openshell_core::resource_admission::DriverAdmissionConfig;
    use openshell_driver_sdk::launch::ImageOverrides;

    use super::*;

    fn gateway(bind: Option<&str>, ca: Option<&str>) -> GatewayContext {
        GatewayContext {
            gateway_port: Some(8080),
            tls_ca_path: ca.map(PathBuf::from),
            gateway_name: Some("gw".to_string()),
            bind_address: bind.map(|address| address.parse().unwrap()),
        }
    }

    fn build(
        settings: DockerComputeConfig,
        gateway: &GatewayContext,
        images: Option<&ImageOverrides>,
        admission: &DriverAdmissionConfig,
    ) -> Result<(DockerComputeConfig, std::net::SocketAddr), Status> {
        driver_config(
            settings,
            gateway,
            images,
            openshell_core::sandbox_env::SupervisorOtlp::default(),
            admission,
        )
    }

    #[test]
    fn the_gateway_supplies_the_listener_name_ca_and_admission() {
        let admission = DriverAdmissionConfig {
            allow_driver_config: true,
            ..DriverAdmissionConfig::default()
        };
        let (config, bind) = build(
            DockerComputeConfig::default(),
            &gateway(Some("127.0.0.1:8080"), Some("/ca.pem")),
            None,
            &admission,
        )
        .unwrap();
        assert_eq!(bind, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.guest_tls_ca, Some("/ca.pem".into()));
        assert_eq!(config.supervisor_gateway_name.as_deref(), Some("gw"));
        assert!(config.allow_driver_config);
    }

    #[test]
    fn the_bind_address_is_required() {
        let status = build(
            DockerComputeConfig::default(),
            &gateway(None, None),
            None,
            &DriverAdmissionConfig::default(),
        )
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn an_https_endpoint_needs_the_gateway_ca() {
        let table = settings_from_table("grpc_endpoint = \"https://host.test:9\"").unwrap();
        let admission = DriverAdmissionConfig::default();
        let status = build(
            table.clone(),
            &gateway(Some("127.0.0.1:8080"), None),
            None,
            &admission,
        )
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        build(
            table,
            &gateway(Some("127.0.0.1:8080"), Some("/ca.pem")),
            None,
            &admission,
        )
        .unwrap();
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
        let (config, _) = build(
            table,
            &gateway(Some("127.0.0.1:8080"), None),
            Some(&overrides),
            &DriverAdmissionConfig::default(),
        )
        .unwrap();
        assert_eq!(config.sandbox_runtime_image.as_deref(), Some("env-runtime"));
        assert_eq!(config.supervisor_image.as_deref(), Some("table-supervisor"));
    }
}
