// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

use std::path::{Path, PathBuf};

use super::*;

fn table(text: &str) -> toml::map::Map<String, toml::Value> {
    toml::from_str(text).unwrap()
}

/// Write an executable script by writing and closing a temporary file, then
/// renaming it into place, so it is never executed while open for writing.
fn driver(dir: &Path, body: &str) {
    let temporary = dir.join("driver.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&temporary)
        .unwrap();
    file.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
        .unwrap();
    file.sync_all().unwrap();
    drop(file);
    std::fs::rename(temporary, dir.join("openshell-driver-podman")).unwrap();
}

fn detect_with(body: &str, extra: &str) -> DriverDetection {
    let dir = tempfile::tempdir().unwrap();
    driver(dir.path(), body);
    let table = table(extra);
    PODMAN.detect(
        &DriverDetectionContext {
            table: &table,
            driver_dir: Some(dir.path()),
            gateway_log_level: "info",
        },
        &dir.path().join("run"),
    )
}

#[test]
fn gateway_owned_keys_are_removed_and_the_rest_is_forwarded() {
    let plan = PODMAN.plan(
        table(
            "driver_dir = \"/opt/drivers\"\nsocket_path = \"/run/podman.sock\"\nnetwork_name = \"n\"",
        ),
        None,
    )
    .unwrap();

    assert_eq!(plan.driver_dir, Some(PathBuf::from("/opt/drivers")));
    let forwarded = table(&plan.table);
    assert!(!forwarded.contains_key("driver_dir"));
    assert_eq!(forwarded["socket_path"].as_str(), Some("/run/podman.sock"));
    assert_eq!(forwarded["network_name"].as_str(), Some("n"));
    assert!(PODMAN.plan(table("driver_dir = 3"), None).is_err());
}

#[test]
fn detection_runs_the_driver_with_the_operators_table() {
    // The fake exits "unavailable" and says which table it received.
    let body = "grep -q 'socket_path = \"/run/user/1/podman.sock\"' || { \
        printf '{\"result\":\"unavailable\",\"reason\":\"wrong table\"}\\n'; exit 69; }\n\
        printf '{\"result\":\"unavailable\",\"reason\":\"right table\"}\\n'; exit 69";
    assert!(matches!(
        detect_with(body, "socket_path = \"/run/user/1/podman.sock\""),
        DriverDetection::Unavailable(reason) if reason == "right table"
    ));
    assert!(matches!(
        detect_with(body, ""),
        DriverDetection::Unavailable(reason) if reason == "wrong table"
    ));
}

#[test]
fn detection_reports_an_invalid_table_so_the_next_candidate_can_be_tried() {
    let body = "printf '{\"result\":\"invalid_config\",\"reason\":\"unknown key\"}\\n'\nexit 78";
    assert!(matches!(
        detect_with(body, ""),
        DriverDetection::InvalidConfig(reason) if reason == "unknown key"
    ));
}

#[test]
fn detection_keeps_the_driver_it_started_and_stops_it_if_unused() {
    // The fake binds the socket named on its command line, as `serve` does.
    if !Path::new("/usr/bin/python3").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let pid_file = dir.path().join("pid");
    driver(
        &bin,
        &format!(
            "echo $$ > '{}'\nexec /usr/bin/python3 -c \"import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(); import time; time.sleep(30)\" \"$3\"",
            pid_file.display()
        ),
    );
    let table = table("");
    let context = DriverDetectionContext {
        table: &table,
        driver_dir: Some(&bin),
        gateway_log_level: "info",
    };

    let result = PODMAN.detect(&context, &dir.path().join("run"));

    let DriverDetection::Available(Some(detected)) = result else {
        panic!("a driver that binds its socket is available and kept");
    };
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    assert!(
        alive(),
        "the detected driver keeps running until it is built"
    );

    // Dropping it without building stops it.
    drop(detected);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!alive(), "an unused detected driver must be stopped");
}

#[test]
fn a_missing_driver_binary_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let table = table("");
    let result = PODMAN.detect(
        &DriverDetectionContext {
            table: &table,
            driver_dir: Some(dir.path()),
            gateway_log_level: "info",
        },
        &dir.path().join("run"),
    );
    assert!(matches!(result, DriverDetection::Unavailable(reason) if reason.contains("not found")));
}

#[test]
fn the_admission_policy_is_not_forwarded_in_the_driver_table() {
    let plan = PODMAN.plan(
        table("allow_driver_config = true\n[resource_admission]\nenabled = false\nrequired_labels = {}\n"),
        None,
    )
    .unwrap();
    assert!(
        !plan.table.contains("allow_driver_config"),
        "{}",
        plan.table
    );
    assert!(!plan.table.contains("resource_admission"), "{}", plan.table);
}

#[test]
fn a_legacy_gateway_port_is_dropped_so_the_driver_can_stay_strict() {
    let plan = PODMAN
        .plan(
            table("gateway_port = 8080\nsocket_path = \"/run/podman.sock\""),
            None,
        )
        .unwrap();
    assert!(!plan.table.contains("gateway_port"), "{}", plan.table);
    assert!(plan.table.contains("socket_path"));
}

#[test]
fn a_launch_error_stops_detection_instead_of_looking_like_an_absent_backend() {
    let dir = tempfile::tempdir().unwrap();
    // Exit status 1 is an internal failure, whatever the driver claims.
    let result = detect_with(
        "printf '{\"result\":\"unavailable\",\"reason\":\"claims absent\"}\\n'\nexit 1",
        "",
    );
    drop(dir);
    assert!(matches!(result, DriverDetection::Failed(reason) if reason.contains("claims absent")));
}

#[test]
fn podman_launches_from_its_own_socket_directory() {
    let podman = PODMAN.socket_dir();
    assert!(podman.ends_with("run/podman"), "{}", podman.display());
}
