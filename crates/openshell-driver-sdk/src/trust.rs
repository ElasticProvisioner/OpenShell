// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::Read as _;
use std::path::PathBuf;

use openshell_core::network_trust::{
    MAX_NETWORK_SUPERVISOR_TRUST_BUNDLE_BYTES, NetworkSupervisorTrustBundle,
};
use openshell_driver_launch::proto;
use tonic::Status;

const CERTIFICATE_BEGIN: &str = "-----BEGIN CERTIFICATE-----";

/// Load the gateway-staged destination trust bundle that `Configure`
/// described, or `None` when the gateway sent none.
///
/// The artifact is read once, without following symlinks and within a size
/// limit. Its digest and certificate count must match the message, and its
/// file permissions must be the ones the gateway sets. The returned bundle
/// keeps the path so a driver can call
/// [`NetworkSupervisorTrustBundle::verify_artifact`] before each sandbox
/// create, start or supervisor replacement.
pub fn load_network_trust(
    trust: Option<&proto::NetworkTrust>,
) -> Result<Option<NetworkSupervisorTrustBundle>, Status> {
    let Some(trust) = trust.filter(|trust| !trust.artifact_path.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(&trust.artifact_path);
    let invalid = |message: String| Status::invalid_argument(message);
    let hex = trust
        .digest
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| invalid("network trust digest must be sha256: and lowercase hex".into()))?;

    let pem = read_artifact(&path)?;
    let actual = openshell_crypto::sha256(&pem)
        .map_err(|error| Status::internal(format!("hash network trust bundle: {error}")))?;
    if hex_lower(&actual) != hex {
        return Err(Status::failed_precondition(format!(
            "network trust artifact '{}' does not match digest {}",
            path.display(),
            trust.digest
        )));
    }
    let count = std::str::from_utf8(&pem)
        .map_err(|_| invalid("network trust bundle is not PEM text".into()))?
        .matches(CERTIFICATE_BEGIN)
        .count();
    if count == 0 || u32::try_from(count) != Ok(trust.certificate_count) {
        return Err(invalid(format!(
            "network trust bundle has {count} certificates but the gateway reported {}",
            trust.certificate_count
        )));
    }
    let bundle = NetworkSupervisorTrustBundle::new(pem, count, trust.digest.clone(), path);
    bundle
        .verify_artifact()
        .map_err(|error| Status::failed_precondition(error.to_string()))?;
    Ok(Some(bundle))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn read_artifact(path: &std::path::Path) -> Result<Vec<u8>, Status> {
    let unreadable = |error: std::io::Error| {
        Status::failed_precondition(format!(
            "read network trust artifact '{}': {error}",
            path.display()
        ))
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(unreadable)?;
    if !file.metadata().map_err(unreadable)?.is_file() {
        return Err(Status::failed_precondition(format!(
            "network trust artifact '{}' is not a regular file",
            path.display()
        )));
    }
    let mut pem = Vec::new();
    file.take(MAX_NETWORK_SUPERVISOR_TRUST_BUNDLE_BYTES as u64 + 1)
        .read_to_end(&mut pem)
        .map_err(unreadable)?;
    if pem.len() > MAX_NETWORK_SUPERVISOR_TRUST_BUNDLE_BYTES {
        return Err(Status::failed_precondition(format!(
            "network trust artifact '{}' is too large",
            path.display()
        )));
    }
    Ok(pem)
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";

    fn stage(mode: u32, contents: &[u8]) -> (tempfile::TempDir, proto::NetworkTrust) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("additional-ca.crt");
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let digest = hex_lower(&openshell_crypto::sha256(contents).unwrap());
        let trust = proto::NetworkTrust {
            artifact_path: path.display().to_string(),
            digest: format!("sha256:{digest}"),
            certificate_count: 1,
        };
        (dir, trust)
    }

    #[test]
    fn no_trust_means_no_bundle() {
        assert!(load_network_trust(None).unwrap().is_none());
        assert!(
            load_network_trust(Some(&proto::NetworkTrust::default()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_staged_artifact_loads_and_verifies() {
        let (_dir, trust) = stage(0o444, PEM);
        let bundle = load_network_trust(Some(&trust)).unwrap().unwrap();
        assert_eq!(bundle.certificate_count(), 1);
        assert_eq!(bundle.digest(), trust.digest);
        bundle.verify_artifact().unwrap();
    }

    #[test]
    fn a_digest_mismatch_is_rejected() {
        let (_dir, mut trust) = stage(0o444, PEM);
        trust.digest = format!("sha256:{}", "0".repeat(64));
        let status = load_network_trust(Some(&trust)).unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn a_wrong_certificate_count_is_rejected() {
        let (_dir, mut trust) = stage(0o444, PEM);
        trust.certificate_count = 2;
        let status = load_network_trust(Some(&trust)).unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_writable_artifact_is_rejected() {
        let (_dir, trust) = stage(0o666, PEM);
        assert!(load_network_trust(Some(&trust)).is_err());
    }

    #[test]
    fn a_symlink_is_not_followed() {
        let (dir, trust) = stage(0o444, PEM);
        let link = dir.path().join("link.crt");
        std::os::unix::fs::symlink(&trust.artifact_path, &link).unwrap();
        let trust = proto::NetworkTrust {
            artifact_path: link.display().to_string(),
            ..trust
        };
        assert!(load_network_trust(Some(&trust)).is_err());
    }

    #[test]
    fn a_malformed_digest_is_rejected() {
        let (_dir, mut trust) = stage(0o444, PEM);
        trust.digest = "sha256:XYZ".to_string();
        assert_eq!(
            load_network_trust(Some(&trust)).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }
}
