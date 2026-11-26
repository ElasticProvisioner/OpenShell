// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use openshell_core::resource_admission::DriverAdmissionConfig;
use tonic::Status;

/// The admission policy the gateway sent in `Configure`.
///
/// Empty means the defaults. The gateway validates the policy before it sends
/// it; the driver checks it again so a malformed message cannot weaken
/// admission.
pub fn admission_from_json(json: &str) -> Result<DriverAdmissionConfig, Status> {
    if json.trim().is_empty() {
        return Ok(DriverAdmissionConfig::default());
    }
    let policy: DriverAdmissionConfig = serde_json::from_str(json)
        .map_err(|error| Status::invalid_argument(format!("invalid admission policy: {error}")))?;
    policy.validate().map_err(Status::invalid_argument)?;
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_means_the_secure_defaults() {
        let policy = admission_from_json("").unwrap();
        assert!(!policy.allow_driver_config);
    }

    #[test]
    fn a_policy_round_trips() {
        let sent = DriverAdmissionConfig {
            allow_driver_config: true,
            ..DriverAdmissionConfig::default()
        };
        let json = serde_json::to_string(&sent).unwrap();
        assert!(admission_from_json(&json).unwrap().allow_driver_config);
    }

    #[test]
    fn malformed_json_is_invalid() {
        let status = admission_from_json("{not json").unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}
