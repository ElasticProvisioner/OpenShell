// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The driver's private Unix-socket listener: socket directory and socket
//! preparation, peer authorization by uid and expected gateway PID, and the
//! shutdown signal.

use futures::Stream;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::net::{UnixListener, UnixStream};
use tracing::info;

pub async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::warn!(%error, "failed to listen for Ctrl-C");
            }
        }
        _ = terminate.recv() => {}
    }
    info!("Shutdown signal received; stopping vm compute driver");
}

pub fn prepare_compute_driver_socket(socket_path: &Path) -> Result<(), String> {
    let Some(parent) = socket_path.parent() else {
        return Err(format!(
            "vm compute driver socket path '{}' has no parent directory",
            socket_path.display()
        ));
    };
    let expected_uid = current_euid();
    prepare_private_socket_dir(parent, expected_uid)?;
    remove_stale_socket(socket_path, expected_uid)
}

pub fn current_euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub fn prepare_private_socket_dir(socket_dir: &Path, expected_uid: u32) -> Result<(), String> {
    std::fs::create_dir_all(socket_dir)
        .map_err(|err| format!("create socket dir {}: {err}", socket_dir.display()))?;
    let metadata = std::fs::symlink_metadata(socket_dir)
        .map_err(|err| format!("stat socket dir {}: {err}", socket_dir.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(format!(
            "socket dir {} is a symlink; refusing to use it",
            socket_dir.display()
        ));
    }
    if !file_type.is_dir() {
        return Err(format!(
            "socket dir {} is not a directory",
            socket_dir.display()
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(format!(
            "socket dir {} is owned by uid {} but current euid is {}",
            socket_dir.display(),
            metadata.uid(),
            expected_uid
        ));
    }
    std::fs::set_permissions(socket_dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("chmod socket dir {}: {err}", socket_dir.display()))
}

pub fn remove_stale_socket(socket_path: &Path, expected_uid: u32) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("stat socket {}: {err}", socket_path.display())),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(format!(
            "socket {} is a symlink; refusing to remove it",
            socket_path.display()
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(format!(
            "socket {} is owned by uid {} but current euid is {}",
            socket_path.display(),
            metadata.uid(),
            expected_uid
        ));
    }
    if !file_type.is_socket() {
        return Err(format!(
            "socket path {} exists but is not a Unix socket",
            socket_path.display()
        ));
    }
    std::fs::remove_file(socket_path)
        .map_err(|err| format!("remove stale socket {}: {err}", socket_path.display()))
}

pub fn restrict_socket_permissions(socket_path: &Path) -> Result<(), String> {
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("chmod socket {}: {err}", socket_path.display()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    uid: u32,
    pid: Option<i32>,
}

pub fn peer_credentials(stream: &UnixStream) -> Result<PeerCredentials, String> {
    let credentials = stream
        .peer_cred()
        .map_err(|err| format!("read peer credentials: {err}"))?;
    Ok(PeerCredentials {
        uid: credentials.uid(),
        pid: credentials.pid(),
    })
}

pub fn authorize_peer_credentials(
    peer: PeerCredentials,
    driver_uid: u32,
    gateway_pid: Option<u32>,
) -> Result<(), String> {
    if peer.uid != driver_uid {
        return Err(format!(
            "peer uid {} does not match current euid {}",
            peer.uid, driver_uid
        ));
    }
    let Some(gateway_pid) = gateway_pid else {
        return Ok(());
    };
    let Some(peer_process_id) = peer.pid.and_then(|pid| u32::try_from(pid).ok()) else {
        return Err(format!(
            "peer pid is unavailable; expected gateway pid {gateway_pid}"
        ));
    };
    if peer_process_id != gateway_pid {
        return Err(format!(
            "peer pid {peer_process_id} does not match expected gateway pid {gateway_pid}"
        ));
    }
    Ok(())
}

pub struct AuthenticatedUnixIncoming {
    listener: UnixListener,
    expected_uid: u32,
    expected_peer_pid: Option<u32>,
}

impl AuthenticatedUnixIncoming {
    pub fn new(listener: UnixListener, expected_peer_pid: Option<u32>) -> Self {
        Self {
            listener,
            expected_uid: current_euid(),
            expected_peer_pid,
        }
    }
}

impl Stream for AuthenticatedUnixIncoming {
    type Item = io::Result<UnixStream>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.listener.poll_accept(cx) {
                Poll::Ready(Ok((stream, _addr))) => {
                    let authorized = peer_credentials(&stream).and_then(|peer| {
                        authorize_peer_credentials(peer, this.expected_uid, this.expected_peer_pid)
                    });
                    match authorized {
                        Ok(()) => return Poll::Ready(Some(Ok(stream))),
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "rejected vm compute driver UDS client"
                            );
                        }
                    }
                }
                Poll::Ready(Err(err)) => return Poll::Ready(Some(Err(err))),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_authorization_accepts_matching_uid_and_pid() {
        authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: Some(42),
            },
            1000,
            Some(42),
        )
        .unwrap();
    }

    #[test]
    fn peer_authorization_rejects_wrong_pid() {
        let err = authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: Some(7),
            },
            1000,
            Some(42),
        )
        .expect_err("wrong pid should be rejected");
        assert!(err.contains("does not match expected gateway pid"));
    }

    #[test]
    fn peer_authorization_rejects_wrong_uid() {
        let err = authorize_peer_credentials(
            PeerCredentials {
                uid: 1001,
                pid: Some(42),
            },
            1000,
            Some(42),
        )
        .expect_err("wrong uid should be rejected");
        assert!(err.contains("does not match current euid"));
    }

    #[test]
    fn peer_authorization_rejects_missing_pid_when_expected() {
        let err = authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: None,
            },
            1000,
            Some(42),
        )
        .expect_err("missing pid should be rejected");
        assert!(err.contains("peer pid is unavailable"));
    }

    #[test]
    fn peer_authorization_accepts_matching_uid_without_expected_pid() {
        authorize_peer_credentials(
            PeerCredentials {
                uid: 1000,
                pid: None,
            },
            1000,
            None,
        )
        .unwrap();
    }
}
