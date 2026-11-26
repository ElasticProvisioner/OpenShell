// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tonic::Code;

use super::proto::driver_launch_client::DriverLaunchClient;
use super::*;

fn info() -> LaunchInfo {
    LaunchInfo {
        driver_name: "test".to_string(),
        driver_version: "1.2.3".to_string(),
        features: vec!["known".to_string()],
    }
}

fn request(version: u32, features: &[&str]) -> ConfigureRequest {
    ConfigureRequest {
        protocol_version: version,
        required_features: features.iter().map(ToString::to_string).collect(),
        ..ConfigureRequest::default()
    }
}

async fn serve<S>(service: S) -> String
where
    S: tower::Service<
            http::Request<tonic::body::Body>,
            Response = http::Response<tonic::body::Body>,
            Error = Infallible,
        > + tonic::server::NamedService
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    endpoint
}

#[tokio::test]
async fn launch_info_answers_and_configure_succeeds_once() {
    let calls = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&calls);
    let launch = Launch::new(info(), move |_| {
        let counted = Arc::clone(&counted);
        async move { Ok(counted.fetch_add(1, Ordering::SeqCst) + 1) }
    });
    let configured = launch.configured();
    let mut client = DriverLaunchClient::connect(serve(launch.into_server()).await)
        .await
        .unwrap();

    let launch_info = client
        .get_launch_info(GetLaunchInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(launch_info.driver_name, "test");
    assert_eq!(launch_info.protocol_versions, [PROTOCOL_VERSION]);
    assert!(configured.get().is_none());

    client
        .configure(request(PROTOCOL_VERSION, &["known"]))
        .await
        .unwrap();
    assert_eq!(configured.get(), Some(&1));
    let again = client
        .configure(request(PROTOCOL_VERSION, &[]))
        .await
        .unwrap_err();
    assert_eq!(again.code(), Code::FailedPrecondition);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unsupported_requests_are_rejected_and_can_be_retried() {
    let launch = Launch::new(info(), |_| async { Ok(()) });
    let configured = launch.configured();
    let mut client = DriverLaunchClient::connect(serve(launch.into_server()).await)
        .await
        .unwrap();

    for bad in [
        request(PROTOCOL_VERSION + 1, &[]),
        request(PROTOCOL_VERSION, &["unknown"]),
    ] {
        let status = client.configure(bad).await.unwrap_err();
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert!(configured.get().is_none());
    }
    client
        .configure(request(PROTOCOL_VERSION, &["known"]))
        .await
        .unwrap();
    assert!(configured.get().is_some());
}

#[tokio::test]
async fn a_failed_configure_can_be_retried() {
    let attempts = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&attempts);
    let launch = Launch::new(info(), move |_| {
        let attempt = counted.fetch_add(1, Ordering::SeqCst);
        async move {
            if attempt == 0 {
                Err(Status::invalid_argument("bad input"))
            } else {
                Ok(())
            }
        }
    });
    let mut client = DriverLaunchClient::connect(serve(launch.into_server()).await)
        .await
        .unwrap();

    let status = client
        .configure(request(PROTOCOL_VERSION, &[]))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    client
        .configure(request(PROTOCOL_VERSION, &[]))
        .await
        .unwrap();
}

#[tokio::test]
async fn oversized_configure_requests_are_rejected() {
    let launch = Launch::new(info(), |_| async { Ok(()) });
    let mut client = DriverLaunchClient::connect(serve(launch.into_server()).await)
        .await
        .unwrap();
    let mut oversized = request(PROTOCOL_VERSION, &[]);
    oversized.telemetry = Some(Telemetry {
        trace_headers: "x".repeat(MAX_CONFIGURE_REQUEST_BYTES),
        ..Telemetry::default()
    });

    let status = client.configure(oversized).await.unwrap_err();
    assert_ne!(status.code(), Code::Ok);
    assert!(decode_configure_request(&vec![0; MAX_CONFIGURE_REQUEST_BYTES + 1]).is_err());
}

#[tokio::test]
async fn deferred_services_wait_for_configure() {
    let launch = Launch::new(info(), |_| async { Ok("configured".to_string()) });
    let configured = launch.configured();
    let mut launch_client = DriverLaunchClient::connect(serve(launch.into_server()).await)
        .await
        .unwrap();
    // The deferred service is a second `DriverLaunch` server, so its answers
    // show which state built it.
    let deferred = Deferred::new(configured, |state: &String| {
        DriverLaunchServer::new(Launch::new(
            LaunchInfo {
                driver_name: state.clone(),
                ..info()
            },
            |_| async { Ok(()) },
        ))
    });
    let mut deferred_client = DriverLaunchClient::connect(serve(deferred).await)
        .await
        .unwrap();

    let before = deferred_client
        .get_launch_info(GetLaunchInfoRequest {})
        .await
        .unwrap_err();
    assert_eq!(before.code(), Code::Unavailable);

    launch_client
        .configure(request(PROTOCOL_VERSION, &[]))
        .await
        .unwrap();
    let after = deferred_client
        .get_launch_info(GetLaunchInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(after.driver_name, "configured");
}

#[test]
fn configure_requests_round_trip() {
    let original = request(PROTOCOL_VERSION, &["known"]);
    let decoded = decode_configure_request(&encode_configure_request(&original)).unwrap();
    assert_eq!(decoded, original);
}

#[tokio::test]
async fn shutdown_sets_the_signal_the_server_stops_on() {
    let launch = Launch::new(info(), |_| async { Ok(()) });
    let shutdown = launch.shutdown();
    assert!(!shutdown.is_requested());
    let mut client = DriverLaunchClient::connect(serve(launch.into_server()).await)
        .await
        .unwrap();

    client.shutdown(ShutdownRequest {}).await.unwrap();

    assert!(shutdown.is_requested());
    // Waiting after the request returns at once.
    tokio::time::timeout(std::time::Duration::from_secs(5), shutdown.requested())
        .await
        .unwrap();
}

#[tokio::test]
async fn requested_waits_until_the_signal_is_set() {
    let shutdown = Shutdown::new();
    let waiting = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { shutdown.requested().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    shutdown.request();

    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
}
