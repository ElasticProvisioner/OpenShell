// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a gateway-launched Docker driver needs before it serves: the one place
//! the operator's table is parsed, and the backend check.

use bollard::Docker;

use crate::{DockerComputeConfig, detect_socket};

/// Settings the gateway supplies through `Configure`, or owns itself, so the
/// table cannot set them. The standalone driver still takes some as flags, so
/// the struct keeps the fields.
const GATEWAY_SUPPLIED_KEYS: &[&str] = &[
    "allow_driver_config",
    "resource_admission",
    "guest_tls_ca",
    "guest_tls_cert",
    "guest_tls_key",
];

/// Parse and validate the operator's `[openshell.drivers.docker]` table.
///
/// This is the only place the table is parsed. It has no side effects: it
/// needs neither the gateway context nor Docker. An empty table selects the
/// defaults.
pub fn settings_from_table(table: &str) -> Result<DockerComputeConfig, String> {
    let keys: toml::Table =
        toml::from_str(table).map_err(|error| format!("invalid docker driver table: {error}"))?;
    if let Some(key) = GATEWAY_SUPPLIED_KEYS
        .iter()
        .find(|key| keys.contains_key(**key))
    {
        return Err(format!(
            "invalid docker driver table: `{key}` is supplied by the gateway and cannot be set here"
        ));
    }
    let config: DockerComputeConfig =
        toml::from_str(table).map_err(|error| format!("invalid docker driver table: {error}"))?;
    if !config.grpc_endpoint.trim().is_empty() {
        openshell_driver_sdk::validate_grpc_endpoint(&config.grpc_endpoint)?;
    }
    config.validate_table().map_err(|error| error.to_string())?;
    Ok(config)
}

/// Check that Docker is reachable with `config`: resolve the socket (the
/// configured one, else the first responsive one) and ping it. Creates
/// nothing.
pub async fn check_backend(config: &DockerComputeConfig) -> Result<(), String> {
    let socket = config
        .socket_path
        .clone()
        .or_else(detect_socket)
        .ok_or_else(|| {
            "no responsive Docker API socket found; set DOCKER_HOST or configure socket_path"
                .to_string()
        })?;
    let socket = socket.to_str().ok_or_else(|| {
        format!(
            "Docker socket path is not valid UTF-8: {}",
            socket.display()
        )
    })?;
    let docker = Docker::connect_with_socket(socket, 30, bollard::API_DEFAULT_VERSION)
        .map_err(|error| format!("failed to create Docker client: {error}"))?;
    docker
        .ping()
        .await
        .map(|_| ())
        .map_err(|error| format!("Docker is not reachable at {socket}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_table_selects_the_defaults() {
        let config = settings_from_table("").unwrap();
        assert!(!config.allow_driver_config);
        assert_eq!(config.sandbox_label, "default");
    }

    #[test]
    fn existing_operator_keys_are_accepted() {
        let config = settings_from_table(
            "sandbox_label = \"mine\"\nimage_pull_policy = \"always\"\npersist_workspace = true\nsocket_path = \"/var/run/docker.sock\"\ngrpc_endpoint = \"http://127.0.0.1:8080\"",
        )
        .unwrap();
        assert_eq!(config.sandbox_label, "mine");
        assert!(config.persist_workspace);
    }

    #[test]
    fn gateway_supplied_keys_are_rejected() {
        for key in [
            "allow_driver_config = true",
            "resource_admission = {}",
            "guest_tls_ca = \"/ca.pem\"",
            "guest_tls_cert = \"/cert.pem\"",
        ] {
            let error = settings_from_table(key).unwrap_err();
            assert!(error.contains("supplied by the gateway"), "{key}: {error}");
        }
    }

    #[test]
    fn unknown_keys_and_bad_endpoints_are_rejected() {
        assert!(settings_from_table("no_such_key = 1").is_err());
        assert!(settings_from_table("grpc_endpoint = \"ftp://gw:1\"").is_err());
    }

    #[tokio::test]
    async fn an_unreachable_socket_is_reported_not_created() {
        let config = settings_from_table("socket_path = \"/nonexistent/docker.sock\"").unwrap();
        let error = check_backend(&config).await.unwrap_err();
        assert!(error.contains("/nonexistent/docker.sock"), "{error}");
    }
}
