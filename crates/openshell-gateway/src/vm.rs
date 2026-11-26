// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! VM compute driver launch.
//!
//! The `openshell-driver-vm` subprocess owns its `[openshell.drivers.vm]`
//! table. This module reads only the two gateway-owned keys, `driver_dir` and
//! `state_dir`, forwards the rest as TOML text on stdin, and launches the
//! driver through [`crate::launcher`].

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use openshell_core::{Error, Result};
use openshell_server::AcquiredRemoteDriverEndpoint;

use crate::launcher::{self, ConfigureInputs, LaunchOutcome, LaunchRequest, PreflightFailure};

const DRIVER_BIN_NAME: &str = "openshell-driver-vm";
const COMPUTE_DRIVER_SOCKET_RUN_DIR: &str = "run";
const DRIVER_BIN_HINT: &str = "set driver_dir in [openshell.gateway], install it in a conventional libexec path such as ~/.local/libexec/openshell, /usr/libexec/openshell, or /usr/local/libexec{,/openshell}, or place it next to the gateway binary";

/// The gateway's view of the `[openshell.drivers.vm]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmLaunchPlan {
    /// Where to look for the driver binary before the conventional locations.
    pub driver_dir: Option<PathBuf>,
    /// The driver's state directory, under which the gateway keeps the
    /// private socket directory.
    pub state_dir: PathBuf,
    /// The table to forward, without gateway-owned keys.
    pub table: String,
}

impl VmLaunchPlan {
    /// Split gateway-owned keys from the table the driver will parse.
    ///
    /// `gateway_driver_dir` is `driver_dir` from `[openshell.gateway]`. A
    /// `driver_dir` in the driver table is deprecated: it is removed with a
    /// warning and used only when the gateway section sets none.
    pub fn from_table(
        mut table: toml::map::Map<String, toml::Value>,
        gateway_driver_dir: Option<&Path>,
    ) -> Result<Self> {
        let path_key = |table: &toml::map::Map<String, toml::Value>, key: &str| {
            table
                .get(key)
                .map(|value| {
                    value.as_str().map(PathBuf::from).ok_or_else(|| {
                        Error::config(format!("[openshell.drivers.vm].{key} must be a string"))
                    })
                })
                .transpose()
        };
        let legacy_driver_dir = path_key(&table, "driver_dir")?;
        if table.remove("driver_dir").is_some() {
            tracing::warn!(
                "[openshell.drivers.vm].driver_dir is deprecated; set driver_dir in [openshell.gateway]"
            );
        }
        let driver_dir = gateway_driver_dir
            .map(Path::to_path_buf)
            .or(legacy_driver_dir);
        let state_dir = path_key(&table, "state_dir")?
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(Self::default_state_dir);
        launcher::strip_gateway_owned_keys(&mut table);
        let table = toml::to_string(&table)
            .map_err(|error| Error::config(format!("invalid vm driver table: {error}")))?;
        Ok(Self {
            driver_dir,
            state_dir,
            table,
        })
    }

    /// Default working directory for VM driver state.
    #[must_use]
    pub fn default_state_dir() -> PathBuf {
        openshell_core::paths::openshell_state_dir().map_or_else(
            |_| PathBuf::from("target/openshell-vm-driver"),
            |dir| dir.join("vm-driver"),
        )
    }

    fn socket_dir(&self) -> PathBuf {
        self.state_dir.join(COMPUTE_DRIVER_SOCKET_RUN_DIR)
    }
}

/// Resolve the trusted `openshell-driver-vm` binary.
///
/// Resolution order: `driver_dir` from `[openshell.drivers.vm]`, or when it
/// is unset the conventional install directories, then the directory of the
/// gateway's own executable so development builds work out of the box.
pub fn resolve_compute_driver_bin(driver_dir: Option<&Path>) -> Result<PathBuf> {
    launcher::resolve_binary(
        DRIVER_BIN_NAME,
        &launcher::driver_search_dirs(driver_dir),
        DRIVER_BIN_HINT,
    )
}

/// Validate the table with the driver's own `preflight`, which contacts no
/// backend and creates no state.
pub fn validate(plan: &VmLaunchPlan) -> Result<()> {
    let binary = resolve_compute_driver_bin(plan.driver_dir.as_deref())?;
    launcher::preflight(&binary, &plan.table).map_err(|failure| match failure {
        PreflightFailure::InvalidConfig(reason) => {
            Error::config(format!("invalid [openshell.drivers.vm] table: {reason}"))
        }
        PreflightFailure::Error(reason) => Error::execution(format!(
            "the vm compute driver failed its preflight: {reason}"
        )),
    })
}

fn prepare_vm_state_dir(state_dir: &Path, expected_uid: u32) -> Result<()> {
    std::fs::create_dir_all(state_dir).map_err(|err| {
        Error::execution(format!(
            "failed to create vm driver state dir '{}': {err}",
            state_dir.display()
        ))
    })?;
    let metadata = checked_directory_metadata(state_dir, expected_uid, "vm driver state dir")?;
    if metadata.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).map_err(
            |err| {
                Error::execution(format!(
                    "failed to restrict vm driver state dir '{}': {err}",
                    state_dir.display()
                ))
            },
        )?;
    }
    Ok(())
}

fn checked_directory_metadata(
    path: &Path,
    expected_uid: u32,
    label: &str,
) -> Result<std::fs::Metadata> {
    let metadata = std::fs::symlink_metadata(path).map_err(|err| {
        Error::execution(format!(
            "failed to stat {label} '{}': {err}",
            path.display()
        ))
    })?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(Error::execution(format!(
            "{label} '{}' is a symlink; refusing to use it",
            path.display()
        )));
    }
    if !file_type.is_dir() {
        return Err(Error::execution(format!(
            "{label} '{}' is not a directory",
            path.display()
        )));
    }
    if metadata.uid() != expected_uid {
        return Err(Error::execution(format!(
            "{label} '{}' is owned by uid {} but current euid is {}",
            path.display(),
            metadata.uid(),
            expected_uid
        )));
    }
    Ok(metadata)
}

/// Launch the VM compute driver, configure it, and return a gRPC channel
/// connected to it with a handle that stops it when dropped.
#[tracing::instrument(
    name = "driver.launch",
    skip_all,
    fields(otel.name = "driver.launch", otel.status_code = tracing::field::Empty, driver.name = "vm")
)]
pub async fn spawn(
    plan: &VmLaunchPlan,
    gateway_log_level: &str,
    inputs: &ConfigureInputs<'_>,
) -> Result<AcquiredRemoteDriverEndpoint> {
    let binary = resolve_compute_driver_bin(plan.driver_dir.as_deref())?;
    prepare_vm_state_dir(&plan.state_dir, rustix::process::geteuid().as_raw())?;
    let configure = inputs.configure_call();
    let outcome = launcher::launch(
        LaunchRequest {
            name: "vm",
            binary: &binary,
            table: &plan.table,
            socket_dir: &plan.socket_dir(),
            log_level: gateway_log_level,
        },
        configure,
        launcher::exit_gateway_on_driver_exit(),
    )
    .await?;
    match outcome {
        LaunchOutcome::Launched(launched) => Ok(AcquiredRemoteDriverEndpoint::managed(
            "vm",
            launched.channel,
            launched.process,
        )),
        LaunchOutcome::InvalidConfig(reason) => Err(Error::config(format!(
            "invalid [openshell.drivers.vm] table: {reason}"
        ))),
        LaunchOutcome::Unavailable(reason) => Err(Error::execution(format!(
            "the vm compute driver is unavailable: {reason}"
        ))),
    }
}

#[cfg(test)]
mod tests;
