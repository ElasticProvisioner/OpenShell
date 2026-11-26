// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Docker compute driver, launched as a subprocess.
//!
//! The `openshell-driver-docker` subprocess owns its
//! `[openshell.drivers.docker]` table. The gateway reads only the keys it
//! owns and forwards the rest as TOML text on stdin; see
//! [`crate::local_driver`].

use openshell_server::{DriverDetection, DriverDetectionContext};

use crate::local_driver::LocalDriver;

pub const DOCKER: LocalDriver = LocalDriver {
    name: "docker",
    drop_legacy_keys: |_| {},
    // The in-process driver did not retry an unreachable daemon.
    retry_unavailable: None,
};

/// Detect Docker by starting its driver with the operator's table, so a
/// configured `socket_path` is honoured.
pub fn detect(context: &DriverDetectionContext<'_>) -> DriverDetection {
    DOCKER.detect(context, &DOCKER.socket_dir())
}

#[cfg(test)]
mod tests;
