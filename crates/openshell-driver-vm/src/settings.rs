// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The operator's `[openshell.drivers.vm]` table.
//!
//! [`settings_from_table`] is the only place the table is parsed and
//! validated. `serve` and `preflight` both call it, so they agree on whether a
//! table is valid. It has no side effects and needs neither the gateway
//! context nor the backend.

use std::path::PathBuf;

use openshell_core::UpstreamProxyConfig;

/// Why a driver table was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Operator-written settings for the VM driver.
///
/// `driver_dir` is gateway-owned: the gateway reads and removes it before it
/// forwards the table, so it is not accepted here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct VmSettings {
    /// Working directory for VM driver sandbox state.
    pub state_dir: PathBuf,
    /// Default sandbox image the driver should use when a request omits one.
    pub default_image: String,
    /// Optional override of the gateway gRPC endpoint the sandbox guest
    /// connects back to, for custom network or NAT setups. Unset, it defaults
    /// from the gateway context. The gateway supplies the gateway port and CA.
    pub grpc_endpoint: String,
    /// Bootstrap image used to boot and prepare VM sandbox target images.
    pub bootstrap_image: String,
    /// Per-image byte limit for host extraction of registry bootstrap layers.
    pub bootstrap_extraction_max_bytes: Option<std::num::NonZeroU64>,
    /// libkrun log level used by the VM driver helper.
    pub krun_log_level: u32,
    /// Default vCPU count for VM sandboxes.
    pub vcpus: u8,
    /// Default memory allocation for VM sandboxes, in MiB.
    pub mem_mib: u32,
    /// Writable overlay disk size for each VM sandbox, in MiB.
    pub overlay_disk_mib: u64,
    /// Optional UID override for the VM guest sandbox account.
    pub sandbox_uid: Option<u32>,
    /// Optional GID override for the VM guest sandbox account.
    pub sandbox_gid: Option<u32>,
    /// Directory shared with the gateway for request-scoped rootfs tar staging.
    pub rootfs_tar_staging_dir: Option<PathBuf>,
    /// Maximum accepted rootfs tar size, in bytes, before and after decompression.
    pub rootfs_tar_max_bytes: Option<u64>,
    /// Corporate forward-proxy settings. Flattening preserves the shared
    /// local-driver TOML field names.
    #[serde(flatten)]
    pub upstream_proxy: UpstreamProxyConfig,
    /// PEM CA bundle trusted for the corporate proxy and for server
    /// certificates re-signed by a TLS-intercepting proxy.
    pub proxy_ca_bundle: Option<PathBuf>,
    /// Policy DNS IPv6 egress mode and NAT64 prefixes for the host-side
    /// supervisors.
    #[serde(flatten)]
    pub ipv6_egress: openshell_core::SupervisorIpv6EgressConfig,
    /// Explicit guest-reachable SPIFFE Workload API TCP listener. VM guests
    /// cannot receive a host UNIX socket, so this requires acknowledgement.
    pub provider_spiffe_workload_api_tcp_endpoint: Option<String>,
    pub provider_spiffe_allow_guest_tcp: bool,
    /// Allow bind-mounting host directories into VM sandboxes.
    pub enable_bind_mounts: bool,
}

impl VmSettings {
    /// Default working directory for VM driver state.
    #[must_use]
    pub fn default_state_dir() -> PathBuf {
        openshell_core::paths::openshell_state_dir().map_or_else(
            |_| PathBuf::from("target/openshell-vm-driver"),
            |dir| dir.join("vm-driver"),
        )
    }

    /// Checks that need neither the gateway context nor the backend.
    fn validate(&self) -> Result<(), ConfigError> {
        self.ipv6_egress.validate().map_err(ConfigError)?;
        if !self.grpc_endpoint.trim().is_empty() {
            openshell_driver_sdk::validate_grpc_endpoint(&self.grpc_endpoint)
                .map_err(ConfigError)?;
        }
        if self.bootstrap_image.trim().is_empty() && self.default_image.trim().is_empty() {
            return Err(ConfigError(
                "bootstrap_image or default_image is required when using the vm compute driver; sandbox images cannot be used as VM bootstrap images".to_string(),
            ));
        }
        let range = openshell_policy::MIN_SANDBOX_UID..=openshell_policy::MAX_SANDBOX_UID;
        for (field, value) in [
            ("sandbox_uid", self.sandbox_uid),
            ("sandbox_gid", self.sandbox_gid),
        ] {
            if let Some(value) = value
                && !range.contains(&value)
            {
                return Err(ConfigError(format!(
                    "{field} {value} is outside the allowed range [{}, {}]",
                    openshell_policy::MIN_SANDBOX_UID,
                    openshell_policy::MAX_SANDBOX_UID
                )));
            }
        }
        if self
            .rootfs_tar_staging_dir
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(ConfigError(
                "rootfs_tar_staging_dir must not be empty when set".to_string(),
            ));
        }
        if self.rootfs_tar_max_bytes == Some(0) {
            return Err(ConfigError(
                "rootfs_tar_max_bytes must be greater than zero when set".to_string(),
            ));
        }
        self.upstream_proxy.validate().map_err(ConfigError)?;
        if let Some(path) = self.proxy_ca_bundle.as_ref() {
            if path.as_os_str().is_empty() {
                return Err(ConfigError(
                    "proxy_ca_bundle must not be empty when set".to_string(),
                ));
            }
            if self.upstream_proxy.https_proxy.is_none() {
                return Err(ConfigError(
                    "proxy_ca_bundle is set but no https_proxy is configured".to_string(),
                ));
            }
        }
        if let Some(endpoint) = self.provider_spiffe_workload_api_tcp_endpoint.as_deref() {
            openshell_core::driver_utils::validate_guest_spiffe_tcp_endpoint(
                endpoint,
                self.provider_spiffe_allow_guest_tcp,
            )
            .map_err(ConfigError)?;
        } else if self.provider_spiffe_allow_guest_tcp {
            return Err(ConfigError(
                "provider_spiffe_allow_guest_tcp is set but no provider_spiffe_workload_api_tcp_endpoint is configured".to_string(),
            ));
        }
        Ok(())
    }
}

impl Default for VmSettings {
    fn default() -> Self {
        Self {
            state_dir: Self::default_state_dir(),
            default_image: openshell_core::image::default_sandbox_image(),
            grpc_endpoint: String::new(),
            bootstrap_image: String::new(),
            bootstrap_extraction_max_bytes: None,
            krun_log_level: 1,
            vcpus: 2,
            mem_mib: 2048,
            overlay_disk_mib: 4096,
            sandbox_uid: None,
            sandbox_gid: None,
            rootfs_tar_staging_dir: None,
            rootfs_tar_max_bytes: None,
            upstream_proxy: UpstreamProxyConfig::default(),
            proxy_ca_bundle: None,
            ipv6_egress: openshell_core::SupervisorIpv6EgressConfig::default(),
            provider_spiffe_workload_api_tcp_endpoint: None,
            provider_spiffe_allow_guest_tcp: false,
            enable_bind_mounts: false,
        }
    }
}

/// Parse and validate the driver table. An empty table is valid and selects
/// the defaults.
pub fn settings_from_table(table: &str) -> Result<VmSettings, ConfigError> {
    settings_with_environment(table, &|_| None)
}

/// [`settings_from_table`] with the standalone driver's environment fallbacks.
///
/// The gateway used to pass only the settings an operator wrote, so a setting
/// left out of the table fell back to the driver's own environment variable.
/// Existing installs rely on that, so a setting the table leaves unset still
/// reads its variable. The table wins when both are set.
pub fn settings_with_environment(
    table: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<VmSettings, ConfigError> {
    let mut settings: VmSettings = toml::from_str(table)
        .map_err(|error| ConfigError(format!("invalid vm driver table: {error}")))?;
    settings.apply_environment(env)?;
    if settings.state_dir.as_os_str().is_empty() {
        settings.state_dir = VmSettings::default_state_dir();
    }
    settings.validate()?;
    Ok(settings)
}

impl VmSettings {
    /// Fill settings the table left unset from their environment variables.
    ///
    /// For `enable_bind_mounts`, `provider_spiffe_allow_guest_tcp` and
    /// `nat64_prefixes`, a `false` or an empty list in the table cannot be told
    /// from an omitted key, so the environment can still enable them, as it
    /// could when the gateway passed flags. #6002 tracks one precedence rule
    /// for these settings.
    fn apply_environment(
        &mut self,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        fn parse<T: std::str::FromStr>(name: &str, value: &str) -> Result<T, ConfigError>
        where
            T::Err: std::fmt::Display,
        {
            value
                .trim()
                .parse()
                .map_err(|error| ConfigError(format!("invalid {name}={value:?}: {error}")))
        }
        let get = |name: &str| env(name).filter(|value| !value.trim().is_empty());

        if self.bootstrap_image.trim().is_empty()
            && let Some(value) = get("OPENSHELL_VM_BOOTSTRAP_IMAGE")
        {
            self.bootstrap_image = value;
        }
        if self.bootstrap_extraction_max_bytes.is_none()
            && let Some(value) = get("OPENSHELL_VM_BOOTSTRAP_EXTRACTION_MAX_BYTES")
        {
            self.bootstrap_extraction_max_bytes = Some(parse(
                "OPENSHELL_VM_BOOTSTRAP_EXTRACTION_MAX_BYTES",
                &value,
            )?);
        }
        if self.sandbox_uid.is_none()
            && let Some(value) = get("OPENSHELL_VM_SANDBOX_UID")
        {
            self.sandbox_uid = Some(parse("OPENSHELL_VM_SANDBOX_UID", &value)?);
        }
        if self.sandbox_gid.is_none()
            && let Some(value) = get("OPENSHELL_VM_SANDBOX_GID")
        {
            self.sandbox_gid = Some(parse("OPENSHELL_VM_SANDBOX_GID", &value)?);
        }
        if self.rootfs_tar_staging_dir.is_none()
            && let Some(value) = get("OPENSHELL_VM_ROOTFS_TAR_STAGING_DIR")
        {
            self.rootfs_tar_staging_dir = Some(value.into());
        }
        if self.rootfs_tar_max_bytes.is_none()
            && let Some(value) = get("OPENSHELL_VM_ROOTFS_TAR_MAX_BYTES")
        {
            self.rootfs_tar_max_bytes = Some(parse("OPENSHELL_VM_ROOTFS_TAR_MAX_BYTES", &value)?);
        }

        let proxy = &mut self.upstream_proxy;
        if proxy.https_proxy.is_none() {
            proxy.https_proxy = get("OPENSHELL_VM_UPSTREAM_PROXY");
        }
        if proxy.no_proxy.is_none() {
            proxy.no_proxy = get("OPENSHELL_VM_UPSTREAM_NO_PROXY");
        }
        if proxy.proxy_auth_file.is_none() {
            proxy.proxy_auth_file = get("OPENSHELL_VM_UPSTREAM_PROXY_AUTH_FILE").map(Into::into);
        }
        // These two acknowledge a proxy setting, and the table validation
        // rejects them when that setting is absent. A `false` in the
        // environment has always meant "not set", so only `true` sets them.
        if proxy.proxy_auth_allow_insecure.is_none()
            && let Some(value) = get("OPENSHELL_VM_UPSTREAM_PROXY_AUTH_ALLOW_INSECURE")
            && parse::<bool>("OPENSHELL_VM_UPSTREAM_PROXY_AUTH_ALLOW_INSECURE", &value)?
        {
            proxy.proxy_auth_allow_insecure = Some(true);
        }
        if proxy.proxy_connect_by_hostname.is_none()
            && let Some(value) = get("OPENSHELL_VM_UPSTREAM_PROXY_CONNECT_BY_HOSTNAME")
            && parse::<bool>("OPENSHELL_VM_UPSTREAM_PROXY_CONNECT_BY_HOSTNAME", &value)?
        {
            proxy.proxy_connect_by_hostname = Some(true);
        }
        if self.proxy_ca_bundle.is_none() {
            self.proxy_ca_bundle = get("OPENSHELL_VM_UPSTREAM_PROXY_CA_BUNDLE").map(Into::into);
        }

        if self.provider_spiffe_workload_api_tcp_endpoint.is_none() {
            self.provider_spiffe_workload_api_tcp_endpoint =
                get("OPENSHELL_PROVIDER_SPIFFE_WORKLOAD_API_TCP_ENDPOINT");
        }
        if !self.provider_spiffe_allow_guest_tcp
            && let Some(value) = get("OPENSHELL_PROVIDER_SPIFFE_ALLOW_GUEST_TCP")
        {
            self.provider_spiffe_allow_guest_tcp =
                parse("OPENSHELL_PROVIDER_SPIFFE_ALLOW_GUEST_TCP", &value)?;
        }
        if !self.enable_bind_mounts
            && let Some(value) = get("OPENSHELL_VM_ENABLE_BIND_MOUNTS")
        {
            self.enable_bind_mounts = parse("OPENSHELL_VM_ENABLE_BIND_MOUNTS", &value)?;
        }

        if self.ipv6_egress.policy_dns_ipv6_egress.is_none()
            && let Some(value) = get("OPENSHELL_VM_POLICY_DNS_IPV6_EGRESS")
        {
            self.ipv6_egress.policy_dns_ipv6_egress =
                Some(value.trim().parse().map_err(|error| {
                    ConfigError(format!("OPENSHELL_VM_POLICY_DNS_IPV6_EGRESS: {error}"))
                })?);
        }
        if self.ipv6_egress.nat64_prefixes.is_empty()
            && let Some(value) = get("OPENSHELL_VM_NAT64_PREFIXES")
        {
            self.ipv6_egress.nat64_prefixes = value
                .split(',')
                .map(|prefix| prefix.trim().to_string())
                .filter(|prefix| !prefix.is_empty())
                .collect();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_table_selects_the_defaults() {
        let settings = settings_from_table("").unwrap();
        assert_eq!(settings.vcpus, 2);
        assert_eq!(settings.mem_mib, 2048);
        assert!(settings.grpc_endpoint.is_empty());
    }

    #[test]
    fn existing_operator_keys_are_accepted() {
        let settings = settings_from_table(
            r#"
            state_dir = "/var/lib/openshell/vm"
            default_image = "example.test/sandbox:1"
            bootstrap_image = "example.test/bootstrap:1"
            grpc_endpoint = "https://gateway.test:8080"
            krun_log_level = 3
            vcpus = 4
            mem_mib = 4096
            overlay_disk_mib = 8192
            sandbox_uid = 10001
            sandbox_gid = 10001
            https_proxy = "http://proxy.test:3128"
            proxy_ca_bundle = "/etc/openshell/proxy-ca.pem"
            enable_bind_mounts = true
            "#,
        )
        .unwrap();
        assert_eq!(settings.vcpus, 4);
        assert_eq!(settings.sandbox_uid, Some(10001));
        assert_eq!(
            settings.upstream_proxy.https_proxy.as_deref(),
            Some("http://proxy.test:3128")
        );
    }

    #[test]
    fn ipv6_egress_settings_are_accepted_and_validated() {
        let settings = settings_from_table(
            "policy_dns_ipv6_egress = \"disabled\"\nnat64_prefixes = [\"2001:db8:64::/96\"]",
        )
        .unwrap();
        assert_eq!(settings.ipv6_egress.nat64_prefixes, ["2001:db8:64::/96"]);
        assert!(settings_from_table("nat64_prefixes = [\"not-a-prefix\"]").is_err());
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn unset_settings_fall_back_to_the_environment() {
        let settings = settings_with_environment(
            "bootstrap_image = \"\"",
            &env(&[
                ("OPENSHELL_VM_UPSTREAM_PROXY", "http://proxy.test:3128"),
                ("OPENSHELL_VM_ROOTFS_TAR_MAX_BYTES", "1048576"),
                ("OPENSHELL_VM_BOOTSTRAP_EXTRACTION_MAX_BYTES", "33554432"),
                ("OPENSHELL_VM_ENABLE_BIND_MOUNTS", "true"),
                ("OPENSHELL_VM_BOOTSTRAP_IMAGE", "example.test/bootstrap:1"),
                (
                    "OPENSHELL_VM_NAT64_PREFIXES",
                    "2001:db8:64::/96, 64:ff9b::/96",
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            settings.upstream_proxy.https_proxy.as_deref(),
            Some("http://proxy.test:3128")
        );
        assert_eq!(settings.rootfs_tar_max_bytes, Some(1_048_576));
        assert_eq!(
            settings.bootstrap_extraction_max_bytes,
            std::num::NonZeroU64::new(33_554_432)
        );
        assert!(settings.enable_bind_mounts);
        assert_eq!(settings.bootstrap_image, "example.test/bootstrap:1");
        assert_eq!(
            settings.ipv6_egress.nat64_prefixes,
            ["2001:db8:64::/96", "64:ff9b::/96"]
        );
    }

    #[test]
    fn an_environment_false_leaves_the_proxy_acknowledgements_unset() {
        let settings = settings_with_environment(
            "",
            &env(&[
                ("OPENSHELL_VM_UPSTREAM_PROXY_AUTH_ALLOW_INSECURE", "false"),
                ("OPENSHELL_VM_UPSTREAM_PROXY_CONNECT_BY_HOSTNAME", "false"),
            ]),
        )
        .unwrap();
        assert_eq!(settings.upstream_proxy.proxy_auth_allow_insecure, None);
        assert_eq!(settings.upstream_proxy.proxy_connect_by_hostname, None);
    }

    #[test]
    fn an_environment_true_sets_the_proxy_acknowledgements() {
        let settings = settings_with_environment(
            "https_proxy = \"http://proxy.test:3128\"\nproxy_auth_file = \"/etc/proxy-auth\"",
            &env(&[
                ("OPENSHELL_VM_UPSTREAM_PROXY_AUTH_ALLOW_INSECURE", "true"),
                ("OPENSHELL_VM_UPSTREAM_PROXY_CONNECT_BY_HOSTNAME", "true"),
            ]),
        )
        .unwrap();
        assert_eq!(
            settings.upstream_proxy.proxy_auth_allow_insecure,
            Some(true)
        );
        assert_eq!(
            settings.upstream_proxy.proxy_connect_by_hostname,
            Some(true)
        );
    }

    #[test]
    fn an_explicit_table_false_is_kept_and_still_needs_its_setting() {
        // The table's value wins over the environment, and `false` with no
        // auth file is rejected, as it was before the driver read the table.
        let error = settings_with_environment(
            "proxy_auth_allow_insecure = false",
            &env(&[("OPENSHELL_VM_UPSTREAM_PROXY_AUTH_ALLOW_INSECURE", "true")]),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("proxy_auth_allow_insecure"),
            "{error}"
        );
    }

    #[test]
    fn the_table_wins_over_the_environment() {
        let settings = settings_with_environment(
            "https_proxy = \"http://table.test:1\"",
            &env(&[("OPENSHELL_VM_UPSTREAM_PROXY", "http://env.test:2")]),
        )
        .unwrap();
        assert_eq!(
            settings.upstream_proxy.https_proxy.as_deref(),
            Some("http://table.test:1")
        );
    }

    #[test]
    fn a_malformed_environment_value_is_invalid_config() {
        let error =
            settings_with_environment("", &env(&[("OPENSHELL_VM_ROOTFS_TAR_MAX_BYTES", "lots")]))
                .unwrap_err();
        assert!(
            error.0.contains("OPENSHELL_VM_ROOTFS_TAR_MAX_BYTES"),
            "{error}"
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = settings_from_table("vcpu = 4").unwrap_err();
        assert!(error.0.contains("vcpu"), "{error}");
    }

    #[test]
    fn gateway_supplied_values_are_not_driver_keys() {
        for key in [
            "guest_tls_ca = \"/ca.pem\"",
            "gateway_port = 8080",
            "allow_driver_config = true",
            "resource_admission = {}",
        ] {
            assert!(settings_from_table(key).is_err(), "accepted: {key}");
        }
    }

    #[test]
    fn grpc_endpoint_is_checked_for_syntax_and_scheme() {
        settings_from_table("grpc_endpoint = \"https://gw.test:8443\"").unwrap();
        for bad in ["ftp://gw.test:1", "gw.test:8080", "http://"] {
            assert!(
                settings_from_table(&format!("grpc_endpoint = \"{bad}\"")).is_err(),
                "accepted: {bad}"
            );
        }
    }

    #[test]
    fn the_gateway_owned_driver_dir_is_not_a_driver_key() {
        assert!(settings_from_table(r#"driver_dir = "/opt/drivers""#).is_err());
    }

    #[test]
    fn invalid_values_are_rejected() {
        for table in [
            "sandbox_uid = 0",
            "rootfs_tar_max_bytes = 0",
            "bootstrap_extraction_max_bytes = 0",
            "rootfs_tar_staging_dir = \"\"",
            "proxy_ca_bundle = \"/ca.pem\"",
            "provider_spiffe_allow_guest_tcp = true",
            "default_image = \"\"\nbootstrap_image = \"\"",
        ] {
            assert!(settings_from_table(table).is_err(), "accepted: {table}");
        }
    }

    #[test]
    fn a_proxy_exclusion_without_a_proxy_names_the_keys() {
        let error = settings_from_table("no_proxy = \"10.0.0.0/8\"").unwrap_err();
        assert!(error.0.contains("no_proxy"), "{error}");
        assert!(error.0.contains("https_proxy"), "{error}");
    }

    #[test]
    fn an_empty_state_dir_selects_the_default() {
        assert_eq!(
            settings_from_table("state_dir = \"\"").unwrap().state_dir,
            VmSettings::default_state_dir()
        );
    }
}
