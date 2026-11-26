// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The only test in its binary, since it installs process-wide tracing.

use openshell_otel::{DeferredExportConfig, Protocol, compute_driver_tracing};
use openshell_otel_test_support::OtlpTestServer;

const TRACE_ID: [u8; 16] = [
    0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd, 0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80, 0x31, 0x9c,
];
const PARENT_SPAN_ID: [u8; 8] = [0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31];
const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

#[tokio::test(flavor = "multi_thread")]
async fn export_starts_after_install_once_and_keeps_the_parent_trace() {
    const TRACING: openshell_otel::ComputeDriverTracing = compute_driver_tracing!();
    let collector = OtlpTestServer::start().await;
    let tracing = openshell_otel::install_deferred_driver_tracing(TRACING, "info", false);

    drop(tracing::info_span!("before-enable", otel.name = "before-enable").entered());

    let config = DeferredExportConfig {
        endpoint: collector.endpoint(),
        protocol: Protocol::Grpc,
        ca_file: None,
        cert_file: None,
        key_file: None,
        gateway_name: Some("test-gateway"),
        service_version: "1.2.3",
        headers: "",
    };
    tracing.enable_export(config).expect("export enables");
    tracing
        .enable_export(config)
        .expect("second call is a no-op");

    // A span for a gateway request: its parent is the gateway's span.
    let span = tracing::info_span!("after-enable", otel.name = "after-enable");
    openshell_otel::set_parent_from_trace_context(&span, Some(TRACEPARENT), None);
    drop(span.entered());

    // Dropping the last handle shuts the provider down, flushing its spans.
    let shutdown = tokio::task::spawn_blocking(move || drop(tracing));
    collector.wait_for_export().await;
    shutdown.await.unwrap();
    let received = collector.shutdown().await;

    let names: Vec<_> = received
        .spans
        .iter()
        .map(|span| span.name.as_str())
        .collect();
    assert_eq!(names, ["after-enable"], "unexpected spans: {names:?}");
    assert_eq!(received.gateway_names, ["test-gateway"]);
    assert_eq!(received.compute_drivers, ["openshell-otel"]);
    let exported = &received.spans[0];
    assert_eq!(exported.trace_id, TRACE_ID, "the gateway's trace id");
    assert_eq!(
        exported.parent_span_id, PARENT_SPAN_ID,
        "the gateway's span"
    );
}
