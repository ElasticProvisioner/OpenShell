// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runs the real `openshell-driver-docker` binary through `serve` and
//! `preflight` against a fake Docker API socket.

#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use hyper_util::rt::TokioIo;
use openshell_core::proto::compute::v1::GetCapabilitiesRequest;
use openshell_core::proto::compute::v1::UpdateSandboxConfigurationRequest;
use openshell_core::proto::compute::v1::compute_driver_client::ComputeDriverClient;
use openshell_core::proto::compute::v1::compute_driver_configuration_updates_client::ComputeDriverConfigurationUpdatesClient;
use openshell_driver_sdk::launch::proto::driver_launch_client::DriverLaunchClient;
use openshell_driver_sdk::launch::{
    ConfigureRequest, GetLaunchInfoRequest, PROTOCOL_VERSION, ShutdownRequest,
};
use tonic::Code;
use tonic::transport::{Channel, Endpoint};

const BIN: &str = env!("CARGO_BIN_EXE_openshell-driver-docker");

fn command(args: &[&str], table: &str, dir: &Path) -> Command {
    // The table is a file, as the gateway provides it, so it always ends.
    let stdin = dir.join("table.toml");
    std::fs::write(&stdin, table).unwrap();
    let mut command = Command::new(BIN);
    command
        .args(args)
        .env_remove("DOCKER_HOST")
        .stdin(std::fs::File::open(stdin).unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

fn run(args: &[&str], table: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    command(args, table, dir.path()).output().unwrap()
}

/// A Docker API that answers every request with `200 OK`.
fn fake_docker(path: &Path) {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nApi-Version: 1.45\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
        }
    });
}

struct Serving {
    child: Child,
    channel: Channel,
    _dir: tempfile::TempDir,
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn connect(path: &Path) -> Option<Channel> {
    let path = path.to_path_buf();
    Endpoint::try_from("http://localhost")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_| {
            let path = path.clone();
            async move {
                Ok::<_, std::io::Error>(TokioIo::new(tokio::net::UnixStream::connect(path).await?))
            }
        }))
        .await
        .ok()
}

async fn serve() -> Serving {
    let dir = tempfile::tempdir().unwrap();
    let docker = dir.path().join("docker.sock");
    fake_docker(&docker);
    let socket = dir.path().join("driver.sock");
    let table = format!("socket_path = \"{}\"\n", docker.display());
    let mut child = command(
        &[
            "serve",
            "--bind-socket",
            socket.to_str().unwrap(),
            "--expected-peer-pid",
            &std::process::id().to_string(),
        ],
        &table,
        dir.path(),
    )
    .spawn()
    .unwrap();
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("driver exited early: {status}");
        }
        if let Some(channel) = connect(&socket).await {
            return Serving {
                child,
                channel,
                _dir: dir,
            };
        }
        assert!(
            Instant::now() < deadline,
            "driver never accepted a connection"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn configure(version: u32) -> ConfigureRequest {
    ConfigureRequest {
        protocol_version: version,
        gateway: Some(openshell_driver_sdk::launch::GatewayContext {
            gateway_port: 8080,
            bind_address: "127.0.0.1:8080".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn launch_info_answers_and_compute_calls_wait_for_configure() {
    let serving = serve().await;

    let info = DriverLaunchClient::new(serving.channel.clone())
        .get_launch_info(GetLaunchInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.driver_name, "docker");
    assert_eq!(info.protocol_versions, [PROTOCOL_VERSION]);

    let status = ComputeDriverClient::new(serving.channel.clone())
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable);

    // The configuration-updates service is gated the same way.
    let status = ComputeDriverConfigurationUpdatesClient::new(serving.channel.clone())
        .update_sandbox_configuration(UpdateSandboxConfigurationRequest::default())
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable);
}

#[tokio::test]
async fn a_failed_configure_leaves_the_driver_running_for_a_retry() {
    let mut serving = serve().await;
    let mut client = DriverLaunchClient::new(serving.channel.clone());

    let unsupported = client
        .configure(configure(PROTOCOL_VERSION + 1))
        .await
        .unwrap_err();
    assert_eq!(unsupported.code(), Code::FailedPrecondition);

    // The fake Docker cannot describe itself, so construction fails here.
    let failed = client
        .configure(configure(PROTOCOL_VERSION))
        .await
        .unwrap_err();
    assert_ne!(failed.code(), Code::Ok);
    assert!(serving.child.try_wait().unwrap().is_none());

    let retried = client
        .configure(configure(PROTOCOL_VERSION))
        .await
        .unwrap_err();
    assert_ne!(retried.code(), Code::FailedPrecondition, "{retried}");
    let still_unavailable = ComputeDriverClient::new(serving.channel.clone())
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .unwrap_err();
    assert_eq!(still_unavailable.code(), Code::Unavailable);
}

fn report_of(output: &Output) -> openshell_driver_sdk::StartReport {
    let stdout = String::from_utf8_lossy(&output.stdout);
    openshell_driver_sdk::StartReport::from_output(&stdout)
        .unwrap_or_else(|| panic!("no report line in: {stdout}"))
}

const SERVE_ARGS: [&str; 5] = [
    "serve",
    "--bind-socket",
    "/nonexistent/driver.sock",
    "--expected-peer-pid",
    "1",
];

#[test]
fn an_invalid_table_exits_with_the_config_code_and_one_json_line() {
    let output = run(&SERVE_ARGS, "no_such_key = 1");
    assert_eq!(output.status.code(), Some(78));
    let report = report_of(&output);
    assert_eq!(report.kind, openshell_driver_sdk::StartKind::InvalidConfig);
    assert!(report.reason.contains("no_such_key"), "{}", report.reason);
}

#[test]
fn an_unreachable_socket_exits_as_unavailable() {
    let output = run(&SERVE_ARGS, "socket_path = \"/nonexistent/docker.sock\"");
    assert_eq!(output.status.code(), Some(69));
    assert_eq!(
        report_of(&output).kind,
        openshell_driver_sdk::StartKind::Unavailable
    );
}

#[test]
fn preflight_accepts_a_valid_table_without_contacting_docker() {
    let table = "socket_path = \"/nonexistent/docker.sock\"\nsandbox_label = \"example\"\n";
    let output = run(&["preflight"], table);
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stdout);
    assert_eq!(run(&["preflight"], "").status.code(), Some(0));
}

#[test]
fn preflight_rejects_an_unknown_key() {
    let output = run(&["preflight"], "no_such_key = 1");
    assert_eq!(output.status.code(), Some(78));
    assert_eq!(
        report_of(&output).kind,
        openshell_driver_sdk::StartKind::InvalidConfig
    );
}

#[test]
fn docker_host_finds_the_socket_when_the_table_sets_none() {
    let dir = tempfile::tempdir().unwrap();
    let docker = dir.path().join("docker.sock");
    fake_docker(&docker);
    let socket = dir.path().join("driver.sock");
    let mut child = command(
        &[
            "serve",
            "--bind-socket",
            socket.to_str().unwrap(),
            "--expected-peer-pid",
            &std::process::id().to_string(),
        ],
        "",
        dir.path(),
    )
    .env("DOCKER_HOST", format!("unix://{}", docker.display()))
    .spawn()
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !socket.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "the driver should have found the socket from DOCKER_HOST"
        );
        assert!(Instant::now() < deadline, "driver never bound its socket");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test]
async fn a_stop_signal_waits_for_the_gateway_to_release_the_driver() {
    let mut serving = serve().await;
    let signalled = Command::new("kill")
        .args(["-TERM", &serving.child.id().to_string()])
        .status()
        .unwrap();
    assert!(signalled.success());

    // The driver defers the signal, so the gateway can still use it.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        serving.child.try_wait().unwrap().is_none(),
        "the driver stopped on SIGTERM before the gateway released it"
    );
    DriverLaunchClient::new(serving.channel.clone())
        .get_launch_info(GetLaunchInfoRequest {})
        .await
        .unwrap();

    DriverLaunchClient::new(serving.channel.clone())
        .shutdown(ShutdownRequest {})
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = serving.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the driver did not stop after Shutdown"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(status.success(), "{status}");
}
