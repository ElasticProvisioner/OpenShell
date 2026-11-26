// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The launched-driver commands: `serve` and `preflight`.
//!
//! The gateway starts `serve` with only bootstrap arguments and the operator's
//! `[openshell.drivers.vm]` table on stdin, then delivers gateway-owned
//! metadata with the `Configure` RPC. No VM state is created before
//! `Configure` succeeds.

use std::path::PathBuf;

use clap::Parser;
use openshell_core::VERSION;
use openshell_core::proto::compute::v1::compute_driver_server::ComputeDriverServer;
use openshell_driver_sdk::launch::{ConfigureRequest, Deferred, Launch, LaunchInfo};
use openshell_driver_sdk::{GatewayContext, StartResult};
use openshell_driver_vm::settings::{VmSettings, settings_with_environment};
use openshell_driver_vm::{VmDriver, VmDriverConfig, procguard};
use tokio::net::UnixListener;
use tonic::Status;
use tracing::info;

/// Bootstrap arguments the gateway passes to `serve`. They are frozen for
/// protocol version 1.
#[derive(Parser, Debug)]
#[command(name = "openshell-driver-vm serve")]
struct ServeArgs {
    #[arg(long)]
    bind_socket: PathBuf,

    #[arg(long)]
    expected_peer_pid: u32,

    #[arg(long, default_value = "info")]
    log_level: String,

    #[arg(long, env = "OPENSHELL_VM_GPU")]
    gpu: bool,

    #[arg(long, env = "OPENSHELL_VM_GPU_MEM_MIB", default_value_t = 8192)]
    gpu_mem_mib: u32,

    #[arg(long, env = "OPENSHELL_VM_GPU_VCPUS", default_value_t = 4)]
    gpu_vcpus: u8,
}

/// Run `serve` or `preflight` when that is the first argument, and return the
/// process exit code. Any other invocation returns `None` and takes the
/// standalone path.
pub async fn dispatch() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1)?.as_str() {
        "serve" => {
            let parsed = ServeArgs::parse_from(
                std::iter::once("openshell-driver-vm serve".to_string())
                    .chain(args.into_iter().skip(2)),
            );
            Some(serve(parsed).await)
        }
        "preflight" => Some(preflight()),
        _ => None,
    }
}

fn read_settings() -> Result<VmSettings, StartResult> {
    let table = openshell_driver_sdk::read_stdin_table()
        .map_err(|error| StartResult::invalid_config(format!("read driver table: {error}")))?;
    settings_with_environment(&table, &|name| std::env::var(name).ok())
        .map_err(|error| StartResult::invalid_config(error.to_string()))
}

fn preflight() -> i32 {
    match read_settings() {
        Ok(_) => 0,
        Err(result) => result.report(),
    }
}

async fn serve(args: ServeArgs) -> i32 {
    let tracing = openshell_otel::install_deferred_driver_tracing(
        openshell_driver_vm::otel_tracing::TRACING,
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
    // The VM driver has no backend to reach before construction, which
    // `Configure` triggers: libkrun and image preparation are used per sandbox.

    crate::listener::prepare_compute_driver_socket(&args.bind_socket)
        .map_err(StartResult::error)?;
    let listener = UnixListener::bind(&args.bind_socket)
        .map_err(|error| StartResult::error(format!("bind socket: {error}")))?;
    crate::listener::restrict_socket_permissions(&args.bind_socket).map_err(StartResult::error)?;

    // Arm procguard so the driver dies with the gateway instead of orphaning
    // its per-sandbox VM launchers.
    if let Err(error) = procguard::die_with_parent() {
        tracing::warn!(%error, "procguard arm failed; gateway crashes may orphan this driver");
    }

    let log_level = args.log_level.clone();
    let launch = Launch::new(
        LaunchInfo {
            driver_name: "vm".to_string(),
            driver_version: VERSION.to_string(),
            features: Vec::new(),
        },
        move |request| {
            let settings = settings.clone();
            let log_level = log_level.clone();
            let tracing = tracing.clone();
            let gpu = (args.gpu, args.gpu_mem_mib, args.gpu_vcpus);
            async move { apply_configure(settings, &log_level, gpu, &tracing, request).await }
        },
    );
    let shutdown = launch.shutdown();
    let configured = launch.configured();
    let deferred = Deferred::new(configured.clone(), |driver: &VmDriver| {
        ComputeDriverServer::new(driver.clone())
    });

    info!(socket = %args.bind_socket.display(), "Starting vm compute driver");
    let result = tonic::transport::Server::builder()
        .layer(openshell_otel::compute_driver_rpc_layer())
        .add_service(launch.into_server())
        .add_service(deferred)
        .serve_with_incoming_shutdown(
            crate::listener::AuthenticatedUnixIncoming::new(listener, Some(args.expected_peer_pid)),
            openshell_driver_sdk::stop_when_released(
                shutdown,
                crate::listener::shutdown_signal(),
                openshell_driver_sdk::EXTERNAL_STOP_GRACE,
            ),
        )
        .await;
    let _ = std::fs::remove_file(&args.bind_socket);
    if let Some(driver) = configured.get() {
        driver.remove_socket_root();
    }
    result.map_err(|error| StartResult::error(format!("serve: {error}")))
}

/// Build the driver from the operator's settings and the gateway's metadata.
async fn apply_configure(
    settings: VmSettings,
    log_level: &str,
    gpu: (bool, u32, u8),
    tracing: &openshell_otel::DeferredDriverTracing,
    request: ConfigureRequest,
) -> Result<VmDriver, Status> {
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
    let admission =
        openshell_driver_sdk::admission_from_json(&request.resource_admission_policy_json)?;
    let config = driver_config(
        settings,
        &gateway,
        request.network_trust.as_ref(),
        supervisor_otlp,
        admission,
        log_level,
        gpu,
    )?;
    VmDriver::new(config).await.map_err(Status::internal)
}

/// The driver configuration for `settings` and the gateway's metadata. An
/// explicit `grpc_endpoint` wins over the gateway context. The gateway
/// supplies the admission policy.
fn driver_config(
    settings: VmSettings,
    gateway: &GatewayContext,
    network_trust: Option<&openshell_driver_sdk::launch::NetworkTrust>,
    supervisor_otlp: openshell_core::sandbox_env::SupervisorOtlp,
    admission: openshell_core::resource_admission::DriverAdmissionConfig,
    log_level: &str,
    (gpu_enabled, gpu_mem_mib, gpu_vcpus): (bool, u32, u8),
) -> Result<VmDriverConfig, Status> {
    let tls_enabled = gateway.tls_ca_path.is_some();
    let grpc_endpoint = if settings.grpc_endpoint.trim().is_empty() {
        let port = gateway.gateway_port.ok_or_else(|| {
            Status::invalid_argument(
                "grpc_endpoint is required when using the vm compute driver and the gateway sent no port",
            )
        })?;
        let scheme = if tls_enabled { "https" } else { "http" };
        format!("{scheme}://127.0.0.1:{port}")
    } else {
        settings.grpc_endpoint
    };
    let trust = network_trust.filter(|trust| !trust.artifact_path.is_empty());
    if openshell_driver_sdk::endpoint_uses_tls(&grpc_endpoint) && !tls_enabled {
        return Err(Status::invalid_argument(
            "grpc_endpoint uses https:// but the gateway supplied no CA path",
        ));
    }
    Ok(VmDriverConfig {
        allow_driver_config: admission.allow_driver_config,
        resource_admission: admission.resource_admission,
        grpc_endpoint,
        supervisor_otlp,
        supervisor_gateway_name: gateway.gateway_name.clone(),
        state_dir: settings.state_dir,
        launcher_bin: None,
        default_image: settings.default_image,
        bootstrap_image: settings.bootstrap_image,
        bootstrap_extraction_max_bytes: settings.bootstrap_extraction_max_bytes,
        log_level: log_level.to_string(),
        krun_log_level: settings.krun_log_level,
        vcpus: settings.vcpus,
        mem_mib: settings.mem_mib,
        overlay_disk_mib: settings.overlay_disk_mib,
        guest_tls_ca: gateway.tls_ca_path.clone(),
        guest_tls_cert: None,
        guest_tls_key: None,
        additional_ca_bundle: trust.map(|trust| PathBuf::from(&trust.artifact_path)),
        additional_ca_digest: trust.map(|trust| trust.digest.clone()),
        additional_ca_certificate_count: trust
            .map(|trust| usize::try_from(trust.certificate_count))
            .transpose()
            .map_err(|_| Status::invalid_argument("certificate_count is out of range"))?,
        additional_ca_snapshot: None,
        upstream_proxy: settings.upstream_proxy,
        proxy_ca_bundle: settings.proxy_ca_bundle,
        ipv6_egress: settings.ipv6_egress,
        provider_spiffe_workload_api_tcp_endpoint: settings
            .provider_spiffe_workload_api_tcp_endpoint,
        provider_spiffe_allow_guest_tcp: settings.provider_spiffe_allow_guest_tcp,
        gpu_enabled,
        gpu_mem_mib,
        gpu_vcpus,
        sandbox_uid: settings.sandbox_uid,
        sandbox_gid: settings.sandbox_gid,
        rootfs_tar_staging_dir: settings.rootfs_tar_staging_dir,
        rootfs_tar_max_bytes: settings.rootfs_tar_max_bytes,
        enable_bind_mounts: settings.enable_bind_mounts,
    })
}

#[cfg(test)]
mod tests {
    use openshell_driver_sdk::launch::NetworkTrust;
    use openshell_driver_vm::settings::settings_from_table;

    use super::*;

    fn config(
        table: &str,
        gateway: &GatewayContext,
        trust: Option<&NetworkTrust>,
    ) -> Result<VmDriverConfig, Status> {
        driver_config(
            settings_from_table(table).unwrap(),
            gateway,
            trust,
            openshell_core::sandbox_env::SupervisorOtlp::default(),
            openshell_core::resource_admission::DriverAdmissionConfig::default(),
            "info",
            (false, 8192, 4),
        )
    }

    #[test]
    fn the_endpoint_defaults_to_the_gateway_loopback_address() {
        let plain = GatewayContext {
            gateway_port: Some(8080),
            ..Default::default()
        };
        assert_eq!(
            config("", &plain, None).unwrap().grpc_endpoint,
            "http://127.0.0.1:8080"
        );
        let tls = GatewayContext {
            gateway_port: Some(8080),
            tls_ca_path: Some("/ca.pem".into()),
            ..Default::default()
        };
        let built = config("", &tls, None).unwrap();
        assert_eq!(built.grpc_endpoint, "https://127.0.0.1:8080");
        assert_eq!(built.guest_tls_ca, Some("/ca.pem".into()));
    }

    #[test]
    fn an_explicit_endpoint_wins_but_the_ca_comes_from_the_gateway() {
        let gateway = GatewayContext {
            gateway_port: Some(8080),
            tls_ca_path: Some("/gateway-ca.pem".into()),
            gateway_name: Some("gw".to_string()),
            bind_address: None,
        };
        let built = config("grpc_endpoint = \"https://host.test:9\"", &gateway, None).unwrap();
        assert_eq!(built.grpc_endpoint, "https://host.test:9");
        assert_eq!(built.guest_tls_ca, Some("/gateway-ca.pem".into()));
        assert_eq!(built.supervisor_gateway_name.as_deref(), Some("gw"));
    }

    #[test]
    fn the_admission_policy_comes_from_configure() {
        let gateway = GatewayContext {
            gateway_port: Some(1),
            ..Default::default()
        };
        let policy = openshell_core::resource_admission::DriverAdmissionConfig {
            allow_driver_config: true,
            ..Default::default()
        };
        let built = driver_config(
            settings_from_table("").unwrap(),
            &gateway,
            None,
            openshell_core::sandbox_env::SupervisorOtlp::default(),
            policy,
            "info",
            (false, 8192, 4),
        )
        .unwrap();
        assert!(built.allow_driver_config);
        assert!(!config("", &gateway, None).unwrap().allow_driver_config);
    }

    #[test]
    fn an_https_endpoint_needs_the_gateway_ca() {
        let gateway = GatewayContext {
            gateway_port: Some(8080),
            ..Default::default()
        };
        let status = config("grpc_endpoint = \"https://host.test:9\"", &gateway, None).unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_missing_endpoint_and_port_is_invalid() {
        let status = config("", &GatewayContext::default(), None).unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn network_trust_reaches_the_driver_config() {
        let gateway = GatewayContext {
            gateway_port: Some(1),
            ..Default::default()
        };
        let trust = NetworkTrust {
            artifact_path: "/var/lib/ca.crt".to_string(),
            digest: "sha256:abc".to_string(),
            certificate_count: 2,
        };
        let built = config("", &gateway, Some(&trust)).unwrap();
        assert_eq!(built.additional_ca_bundle, Some("/var/lib/ca.crt".into()));
        assert_eq!(built.additional_ca_digest.as_deref(), Some("sha256:abc"));
        assert_eq!(built.additional_ca_certificate_count, Some(2));
        assert!(
            config("", &gateway, None)
                .unwrap()
                .additional_ca_bundle
                .is_none()
        );
    }
}
