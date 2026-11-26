// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::path::PathBuf;

use openshell_driver_launch::proto;
use tonic::Status;

/// Gateway-owned values a driver needs. A launched driver receives them in
/// `Configure`. Unset values fall back to what the operator's driver table
/// says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayContext {
    pub gateway_port: Option<u16>,
    pub tls_ca_path: Option<PathBuf>,
    pub gateway_name: Option<String>,
    /// The gateway's listener, which a driver may use to choose how sandboxes
    /// reach it.
    pub bind_address: Option<SocketAddr>,
}

impl TryFrom<&proto::GatewayContext> for GatewayContext {
    type Error = Status;

    fn try_from(context: &proto::GatewayContext) -> Result<Self, Status> {
        let gateway_port = match context.gateway_port {
            0 => None,
            port => Some(u16::try_from(port).map_err(|_| {
                Status::invalid_argument(format!("gateway_port {port} is out of range"))
            })?),
        };
        let non_empty = |value: &str| (!value.is_empty()).then(|| value.to_string());
        let bind_address = non_empty(&context.bind_address)
            .map(|address| {
                address.parse().map_err(|error| {
                    Status::invalid_argument(format!("bind_address {address:?}: {error}"))
                })
            })
            .transpose()?;
        Ok(Self {
            gateway_port,
            tls_ca_path: non_empty(&context.tls_ca_path).map(PathBuf::from),
            gateway_name: non_empty(&context.gateway_name),
            bind_address,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_values_are_unset() {
        assert_eq!(
            GatewayContext::try_from(&proto::GatewayContext::default()).unwrap(),
            GatewayContext::default()
        );
    }

    #[test]
    fn values_are_carried_over() {
        let context = GatewayContext::try_from(&proto::GatewayContext {
            gateway_port: 8080,
            tls_ca_path: "/etc/ca.pem".to_string(),
            gateway_name: "gw".to_string(),
            bind_address: "127.0.0.1:8080".to_string(),
        })
        .unwrap();
        assert_eq!(context.gateway_port, Some(8080));
        assert_eq!(context.tls_ca_path, Some(PathBuf::from("/etc/ca.pem")));
        assert_eq!(context.gateway_name.as_deref(), Some("gw"));
        assert_eq!(
            context.bind_address,
            Some("127.0.0.1:8080".parse().unwrap())
        );
    }

    #[test]
    fn a_malformed_bind_address_is_invalid() {
        let status = GatewayContext::try_from(&proto::GatewayContext {
            bind_address: "not-an-address".to_string(),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn an_out_of_range_port_is_invalid() {
        let status = GatewayContext::try_from(&proto::GatewayContext {
            gateway_port: 70_000,
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}
