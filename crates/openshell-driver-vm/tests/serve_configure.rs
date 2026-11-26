// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runs the real `openshell-driver-vm` binary through `serve` and `preflight`.

#![cfg(unix)]

use std::io::Write as _;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use hyper_util::rt::TokioIo;
use openshell_core::proto::compute::v1::GetCapabilitiesRequest;
use openshell_core::proto::compute::v1::compute_driver_client::ComputeDriverClient;
use openshell_driver_sdk::launch::proto::driver_launch_client::DriverLaunchClient;
use openshell_driver_sdk::launch::{
    ConfigureRequest, GetLaunchInfoRequest, PROTOCOL_VERSION, ShutdownRequest,
};
use tonic::Code;
use tonic::transport::{Channel, Endpoint};

const BIN: &str = env!("CARGO_BIN_EXE_openshell-driver-vm");

fn driver(args: &[&str], table: &str) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let _ = table;
    command
}

fn run_with_table(args: &[&str], table: &str) -> Output {
    let mut child = driver(args, table).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(table.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
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

async fn serve(table: &str) -> Serving {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("run").join("driver.sock");
    let table = table.replace("@STATE@", &dir.path().join("state").display().to_string());
    let pid = std::process::id().to_string();
    let mut child = driver(
        &[
            "serve",
            "--bind-socket",
            socket.to_str().unwrap(),
            "--expected-peer-pid",
            &pid,
        ],
        &table,
    )
    .spawn()
    .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(table.as_bytes())
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

const TABLE: &str = "state_dir = \"@STATE@\"\ndefault_image = \"example.test/sandbox:1\"\n";

fn configure(version: u32, port: u32) -> ConfigureRequest {
    ConfigureRequest {
        protocol_version: version,
        gateway: Some(openshell_driver_sdk::launch::GatewayContext {
            gateway_port: port,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn launch_info_answers_and_compute_calls_wait_for_configure() {
    let serving = serve(TABLE).await;

    let info = DriverLaunchClient::new(serving.channel.clone())
        .get_launch_info(GetLaunchInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.driver_name, "vm");
    assert_eq!(info.protocol_versions, [PROTOCOL_VERSION]);

    let status = ComputeDriverClient::new(serving.channel.clone())
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable);
}

#[tokio::test]
async fn a_failed_configure_leaves_the_driver_running_for_a_retry() {
    let mut serving = serve(TABLE).await;
    let mut client = DriverLaunchClient::new(serving.channel.clone());

    let unsupported = client
        .configure(configure(PROTOCOL_VERSION + 1, 8080))
        .await
        .unwrap_err();
    assert_eq!(unsupported.code(), Code::FailedPrecondition);

    // No gateway port and no `grpc_endpoint` in the table.
    let incomplete = client
        .configure(configure(PROTOCOL_VERSION, 0))
        .await
        .unwrap_err();
    assert_eq!(incomplete.code(), Code::InvalidArgument);
    assert!(serving.child.try_wait().unwrap().is_none());

    client
        .configure(configure(PROTOCOL_VERSION, 8080))
        .await
        .unwrap();
    // The driver now answers; it rejects this bare request only because the
    // extension handshake metadata is missing.
    let answered = ComputeDriverClient::new(serving.channel.clone())
        .get_capabilities(GetCapabilitiesRequest::default())
        .await;
    assert_ne!(
        answered.err().map(|status| status.code()),
        Some(Code::Unavailable)
    );
    let again = client
        .configure(configure(PROTOCOL_VERSION, 8080))
        .await
        .unwrap_err();
    assert_eq!(again.code(), Code::FailedPrecondition);
}

#[test]
fn an_invalid_table_exits_with_the_config_code_and_one_json_line() {
    let output = run_with_table(
        &[
            "serve",
            "--bind-socket",
            "/nonexistent/driver.sock",
            "--expected-peer-pid",
            "1",
        ],
        "vcpu = 4",
    );
    assert_eq!(output.status.code(), Some(78));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report = openshell_driver_sdk::StartReport::from_output(&stdout)
        .unwrap_or_else(|| panic!("no report line in: {stdout}"));
    assert_eq!(report.kind, openshell_driver_sdk::StartKind::InvalidConfig);
    assert!(report.reason.contains("vcpu"), "{}", report.reason);
}

#[test]
fn preflight_accepts_a_valid_table_and_creates_no_state() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let table = format!("state_dir = \"{}\"\nvcpus = 4\n", state.display());

    let output = run_with_table(&["preflight"], &table);
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stdout);
    assert!(!state.exists(), "preflight must not create state");
    assert_eq!(run_with_table(&["preflight"], "").status.code(), Some(0));
}

#[test]
fn preflight_rejects_an_unknown_key() {
    let output = run_with_table(&["preflight"], "vcpu = 4");
    assert_eq!(output.status.code(), Some(78));
    assert!(
        openshell_driver_sdk::StartReport::from_output(&String::from_utf8_lossy(&output.stdout))
            .is_some()
    );
}

#[tokio::test]
async fn a_stop_signal_waits_for_the_gateway_to_release_the_driver() {
    let mut serving = serve("").await;
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
