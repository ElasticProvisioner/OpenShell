// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::{Pid, getppid};
use openshell_driver_launch::Shutdown;

/// How often the guard checks that the parent still exists.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long a driver whose gateway is gone has to finish before it exits.
const ORPHAN_EXIT_BACKSTOP: Duration = Duration::from_secs(10);

/// Release the driver when `parent_pid` is gone.
///
/// A driver does not outlive the gateway that launched it. This works on
/// every Unix, unlike `PR_SET_PDEATHSIG`. The driver then finishes the calls in
/// flight and exits; if it has not stopped within a few seconds, the guard
/// exits it.
///
/// A process that no longer exists is not the only sign. When the gateway
/// started this driver directly, the driver is reparented once the gateway
/// exits, so a changed parent also counts. That still holds if the operating
/// system gives the gateway's PID to another process.
pub fn watch_parent(parent_pid: u32, shutdown: Shutdown) {
    let Ok(parent_pid) = i32::try_from(parent_pid) else {
        return;
    };
    let pid = Pid::from_raw(parent_pid);
    let direct_child = getppid() == pid;
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(POLL_INTERVAL);
            let exists = kill(pid, None) != Err(Errno::ESRCH);
            if parent_is_gone(exists, direct_child, getppid(), pid) {
                eprintln!("parent process {parent_pid} exited; stopping");
                shutdown.request();
                std::thread::sleep(ORPHAN_EXIT_BACKSTOP);
                std::process::exit(1);
            }
        }
    });
}

fn parent_is_gone(exists: bool, direct_child: bool, current_parent: Pid, expected: Pid) -> bool {
    !exists || (direct_child && current_parent != expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_parent_is_gone() {
        assert!(parent_is_gone(
            false,
            false,
            Pid::from_raw(7),
            Pid::from_raw(7)
        ));
    }

    #[test]
    fn a_reparented_direct_child_is_gone_even_if_the_pid_is_reused() {
        let expected = Pid::from_raw(7);
        assert!(parent_is_gone(true, true, Pid::from_raw(1), expected));
        assert!(!parent_is_gone(true, true, expected, expected));
    }

    #[test]
    fn a_driver_behind_a_wrapper_relies_on_the_pid_alone() {
        assert!(!parent_is_gone(
            true,
            false,
            Pid::from_raw(1),
            Pid::from_raw(7)
        ));
    }
}
