// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use openshell_core::sandbox_env::{OtlpExport, OtlpProtocol, SupervisorOtlp};
use openshell_driver_launch::proto;
use openshell_otel::DeferredExportConfig;
use tonic::Status;

fn export(export: Option<&proto::OtlpExport>) -> Result<Option<OtlpExport>, Status> {
    let Some(export) = export.filter(|export| !export.endpoint.is_empty()) else {
        return Ok(None);
    };
    let protocol = if export.protocol.is_empty() {
        OtlpProtocol::default()
    } else {
        export.protocol.parse().map_err(Status::invalid_argument)?
    };
    Ok(Some(OtlpExport {
        endpoint: export.endpoint.clone(),
        protocol,
    }))
}

/// The OTLP exports supervisors receive.
pub fn supervisor_otlp(telemetry: Option<&proto::Telemetry>) -> Result<SupervisorOtlp, Status> {
    let Some(telemetry) = telemetry else {
        return Ok(SupervisorOtlp::default());
    };
    Ok(SupervisorOtlp {
        traces: export(telemetry.traces.as_ref())?,
        ocsf: export(telemetry.ocsf.as_ref())?,
        agent: export(telemetry.agent.as_ref())?,
    })
}

/// The settings for the driver's own trace exporter, or `None` when the
/// gateway configured no trace endpoint.
pub fn deferred_export_config<'a>(
    telemetry: Option<&'a proto::Telemetry>,
    gateway_name: Option<&'a str>,
    service_version: &'static str,
) -> Result<Option<DeferredExportConfig<'a>>, Status> {
    let Some(telemetry) = telemetry else {
        return Ok(None);
    };
    let Some(traces) = export(telemetry.traces.as_ref())? else {
        return Ok(None);
    };
    let tls = telemetry.collector.as_ref();
    let path = |value: Option<&'a String>| {
        value
            .filter(|value| !value.is_empty())
            .map(|value| Path::new(value.as_str()))
    };
    Ok(Some(DeferredExportConfig {
        endpoint: telemetry
            .traces
            .as_ref()
            .map_or("", |traces| traces.endpoint.as_str()),
        protocol: traces.protocol.into(),
        ca_file: path(tls.map(|tls| &tls.ca_file)),
        cert_file: path(tls.map(|tls| &tls.cert_file)),
        key_file: path(tls.map(|tls| &tls.key_file)),
        gateway_name,
        service_version,
        headers: telemetry.trace_headers.as_str(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telemetry() -> proto::Telemetry {
        proto::Telemetry {
            traces: Some(proto::OtlpExport {
                endpoint: "http://traces:4317".to_string(),
                protocol: "http/protobuf".to_string(),
            }),
            ocsf: Some(proto::OtlpExport {
                endpoint: "http://ocsf:4317".to_string(),
                protocol: String::new(),
            }),
            agent: None,
            collector: Some(proto::CollectorTls {
                ca_file: "/etc/ca.pem".to_string(),
                ..Default::default()
            }),
            trace_headers: "authorization=secret".to_string(),
        }
    }

    #[test]
    fn supervisors_receive_each_lane_with_its_protocol() {
        let otlp = supervisor_otlp(Some(&telemetry())).unwrap();
        assert_eq!(
            otlp.traces,
            Some(OtlpExport {
                endpoint: "http://traces:4317".to_string(),
                protocol: OtlpProtocol::HttpProtobuf
            })
        );
        assert_eq!(otlp.ocsf.unwrap().protocol, OtlpProtocol::Grpc);
        assert!(otlp.agent.is_none());
        assert_eq!(supervisor_otlp(None).unwrap(), SupervisorOtlp::default());
    }

    #[test]
    fn an_unknown_protocol_is_invalid() {
        let mut telemetry = telemetry();
        telemetry.traces.as_mut().unwrap().protocol = "carrier-pigeon".to_string();
        let status = supervisor_otlp(Some(&telemetry)).unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn the_driver_exporter_takes_the_trace_lane_tls_and_headers() {
        let telemetry = telemetry();
        let config = deferred_export_config(Some(&telemetry), Some("gw"), "1.0")
            .unwrap()
            .unwrap();
        assert_eq!(config.endpoint, "http://traces:4317");
        assert_eq!(config.ca_file, Some(Path::new("/etc/ca.pem")));
        assert_eq!(config.cert_file, None);
        assert_eq!(config.gateway_name, Some("gw"));
        assert_eq!(config.headers, "authorization=secret");
    }

    #[test]
    fn no_trace_endpoint_means_no_exporter() {
        let mut telemetry = telemetry();
        telemetry.traces = None;
        assert!(
            deferred_export_config(Some(&telemetry), None, "1.0")
                .unwrap()
                .is_none()
        );
        assert!(deferred_export_config(None, None, "1.0").unwrap().is_none());
    }
}
