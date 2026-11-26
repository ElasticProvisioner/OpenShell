// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! What the gateway does the same way for each local driver it launches and
//! detects: split gateway-owned keys from the table, find the binary, check
//! the table with the driver's `preflight`, detect it by starting it, and
//! launch and configure it.
//!
//! The gateway knows only the driver's name and the keys it owns or has
//! deprecated. It never parses the rest of the table.

use std::path::{Path, PathBuf};

use openshell_core::{Error, Result};
use openshell_server::{
    AcquiredRemoteDriverEndpoint, DetectedDriver, DriverDetection, DriverDetectionContext,
};

use crate::launcher::{
    self, ConfigureInputs, LaunchOutcome, LaunchRequest, PreflightFailure, StartOutcome, Started,
    UnavailableRetry,
};

/// One launched driver, such as `podman`.
#[derive(Clone, Copy)]
pub struct LocalDriver {
    /// The driver's name, which names its table and binary.
    pub name: &'static str,
    /// Removes keys earlier releases accepted that the driver's strict table no
    /// longer does. The gateway warns about each, so the driver can reject them.
    pub drop_legacy_keys: fn(&mut toml::map::Map<String, toml::Value>),
    /// Start a named driver again this often while its backend is unreachable.
    /// Drivers that never retried leave it unset.
    pub retry_unavailable: Option<UnavailableRetry>,
}

/// The gateway's view of a driver's table: where its binary is, and the table
/// to forward, without gateway-owned keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub driver_dir: Option<PathBuf>,
    pub table: String,
}

impl LocalDriver {
    fn binary_name(self) -> String {
        format!("openshell-driver-{}", self.name)
    }

    /// Split gateway-owned keys from the table the driver will parse.
    ///
    /// `gateway_driver_dir` is `driver_dir` from `[openshell.gateway]`. A
    /// `driver_dir` in the driver table is deprecated: it is removed with a
    /// warning and used only when the gateway section sets none. The admission
    /// policy is removed too: the gateway owns it and sends it in `Configure`.
    pub fn plan(
        self,
        mut table: toml::map::Map<String, toml::Value>,
        gateway_driver_dir: Option<&Path>,
    ) -> Result<LaunchPlan> {
        let name = self.name;
        let legacy_driver_dir = table
            .remove("driver_dir")
            .map(|value| {
                tracing::warn!(
                    "[openshell.drivers.{name}].driver_dir is deprecated; set driver_dir in [openshell.gateway]"
                );
                value.as_str().map(PathBuf::from).ok_or_else(|| {
                    Error::config(format!("[openshell.drivers.{name}].driver_dir must be a string"))
                })
            })
            .transpose()?;
        let driver_dir = gateway_driver_dir
            .map(Path::to_path_buf)
            .or(legacy_driver_dir);
        launcher::strip_gateway_owned_keys(&mut table);
        (self.drop_legacy_keys)(&mut table);
        let table = toml::to_string(&table)
            .map_err(|error| Error::config(format!("invalid {name} driver table: {error}")))?;
        Ok(LaunchPlan { driver_dir, table })
    }

    fn binary(self, plan: &LaunchPlan) -> Result<PathBuf> {
        let binary = self.binary_name();
        launcher::resolve_binary(
            &binary,
            &launcher::driver_search_dirs(plan.driver_dir.as_deref()),
            "set driver_dir in [openshell.gateway], install it in a conventional libexec path such as ~/.local/libexec/openshell, /usr/libexec/openshell, or /usr/local/libexec{,/openshell}, or place it next to the gateway binary",
        )
    }

    /// Validate the table with the driver's own `preflight`, which contacts no
    /// backend and creates no state.
    pub fn validate(self, plan: &LaunchPlan) -> Result<()> {
        let name = self.name;
        launcher::preflight(&self.binary(plan)?, &plan.table).map_err(|failure| match failure {
            PreflightFailure::InvalidConfig(reason) => Error::config(format!(
                "invalid [openshell.drivers.{name}] table: {reason}"
            )),
            PreflightFailure::Error(reason) => Error::execution(format!(
                "the {name} compute driver failed its preflight: {reason}"
            )),
        })
    }

    /// Decide whether the driver can serve with the operator's table by
    /// starting it, which makes it check the table and its backend before it
    /// binds. The running driver is kept: if it is selected, this same process
    /// is configured.
    pub fn detect(
        self,
        context: &DriverDetectionContext<'_>,
        socket_dir: &Path,
    ) -> DriverDetection {
        let plan = match self.plan(context.table.clone(), context.driver_dir) {
            Ok(plan) => plan,
            Err(error) => return DriverDetection::InvalidConfig(error.to_string()),
        };
        let binary = match self.binary(&plan) {
            Ok(binary) => binary,
            Err(error) => return DriverDetection::Unavailable(error.to_string()),
        };
        match launcher::start(&LaunchRequest {
            name: self.name,
            binary: &binary,
            table: &plan.table,
            socket_dir,
            log_level: context.gateway_log_level,
        }) {
            Ok(StartOutcome::Started(started)) => {
                DriverDetection::Available(Some(DetectedDriver::new(started)))
            }
            Ok(StartOutcome::Unavailable(reason)) => DriverDetection::Unavailable(reason),
            Ok(StartOutcome::InvalidConfig(reason)) => DriverDetection::InvalidConfig(reason),
            // A crash, a timeout, or a launch error is not an absent backend.
            Err(error) => DriverDetection::Failed(error.to_string()),
        }
    }

    /// Launch the driver, configure it, and return a gRPC channel connected to
    /// it with a handle that stops it when dropped. `detected` is the driver
    /// that auto-detection already started, which is configured instead of
    /// starting another.
    pub async fn spawn(
        self,
        plan: &LaunchPlan,
        detected: Option<Started>,
        socket_dir: &Path,
        gateway_log_level: &str,
        inputs: &ConfigureInputs<'_>,
    ) -> Result<AcquiredRemoteDriverEndpoint> {
        let name = self.name;
        let configure = inputs.configure_call();
        let on_exit = launcher::exit_gateway_on_driver_exit();
        let launched = if let Some(started) = detected {
            launcher::finish(started, configure, on_exit).await?
        } else {
            let binary = self.binary(plan)?;
            let request = LaunchRequest {
                name,
                binary: &binary,
                table: &plan.table,
                socket_dir,
                log_level: gateway_log_level,
            };
            let outcome = match self.retry_unavailable {
                Some(retry) => {
                    launcher::launch_tolerating_unavailable(request, configure, on_exit, retry)
                        .await?
                }
                None => launcher::launch(request, configure, on_exit).await?,
            };
            match outcome {
                LaunchOutcome::Launched(launched) => launched,
                LaunchOutcome::InvalidConfig(reason) => {
                    return Err(Error::config(format!(
                        "invalid [openshell.drivers.{name}] table: {reason}"
                    )));
                }
                LaunchOutcome::Unavailable(reason) => {
                    return Err(Error::execution(format!(
                        "the {name} compute driver is unavailable: {reason}"
                    )));
                }
            }
        };
        Ok(AcquiredRemoteDriverEndpoint::managed(
            name,
            launched.channel,
            launched.process,
        ))
    }

    /// The directory for this driver's launch sockets. The launcher owns it
    /// and restricts it to the gateway's user, so nothing else should use it.
    #[must_use]
    pub fn socket_dir(self) -> PathBuf {
        let name = self.name;
        openshell_core::paths::openshell_state_dir().map_or_else(
            |_| PathBuf::from("target/openshell/run").join(name),
            |dir| dir.join("run").join(name),
        )
    }
}

/// How long to wait between starts while a named driver's backend is down.
#[cfg(feature = "compute-driver-podman")]
pub const BACKEND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
