// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::fs::PermissionsExt as _;

use tempfile::tempdir;

use super::*;

fn table(text: &str) -> toml::map::Map<String, toml::Value> {
    toml::from_str(text).unwrap()
}

#[test]
fn gateway_owned_keys_are_removed_and_the_rest_is_forwarded() {
    let plan = VmLaunchPlan::from_table(
        table(
            "driver_dir = \"/opt/drivers\"\nstate_dir = \"/var/lib/vm\"\nvcpus = 4\nunknown_to_the_gateway = true",
        ),
        None,
    )
    .unwrap();

    assert_eq!(plan.driver_dir, Some(PathBuf::from("/opt/drivers")));
    assert_eq!(plan.state_dir, PathBuf::from("/var/lib/vm"));
    let forwarded: toml::map::Map<String, toml::Value> = toml::from_str(&plan.table).unwrap();
    assert!(!forwarded.contains_key("driver_dir"));
    assert_eq!(forwarded["state_dir"].as_str(), Some("/var/lib/vm"));
    assert_eq!(forwarded["vcpus"].as_integer(), Some(4));
    assert_eq!(forwarded["unknown_to_the_gateway"].as_bool(), Some(true));
}

#[test]
fn an_empty_table_selects_the_default_state_dir() {
    let plan = VmLaunchPlan::from_table(table(""), None).unwrap();
    assert_eq!(plan.state_dir, VmLaunchPlan::default_state_dir());
    assert_eq!(plan.driver_dir, None);
    assert!(plan.table.trim().is_empty());

    let empty = VmLaunchPlan::from_table(table("state_dir = \"\""), None).unwrap();
    assert_eq!(empty.state_dir, VmLaunchPlan::default_state_dir());
}

#[test]
fn a_non_string_gateway_key_is_rejected() {
    assert!(VmLaunchPlan::from_table(table("driver_dir = 3"), None).is_err());
}

#[test]
fn socket_dir_is_the_private_run_dir_under_the_state_dir() {
    let plan =
        VmLaunchPlan::from_table(table("state_dir = \"/tmp/openshell-vm-state\""), None).unwrap();
    assert_eq!(
        plan.socket_dir(),
        PathBuf::from("/tmp/openshell-vm-state/run")
    );
}

#[test]
fn resolve_driver_bin_uses_driver_dir_when_binary_present() {
    let dir = tempdir().unwrap();
    let driver = dir.path().join(DRIVER_BIN_NAME);
    std::fs::write(&driver, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        resolve_compute_driver_bin(Some(dir.path())).unwrap(),
        driver
    );
}

#[test]
fn resolve_driver_bin_error_mentions_driver_dir_hint() {
    let dir = tempdir().unwrap();
    let error = resolve_compute_driver_bin(Some(dir.path()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("driver_dir"), "{error}");
    assert!(error.contains("openshell-driver-vm"), "{error}");
}

#[test]
fn resolve_driver_search_dirs_include_libexec_fallbacks() {
    let dirs = launcher::default_driver_search_dirs(Some(PathBuf::from("/home/openshell")));
    assert_eq!(
        dirs.first(),
        Some(&PathBuf::from("/home/openshell/.local/libexec/openshell"))
    );
    assert!(dirs.contains(&PathBuf::from("/usr/libexec/openshell")));
    assert!(dirs.contains(&PathBuf::from("/usr/local/libexec/openshell")));
    assert!(dirs.contains(&PathBuf::from("/usr/local/libexec")));
}

#[test]
fn prepare_vm_state_dir_creates_and_restricts_the_directory() {
    let dir = tempdir().unwrap();
    let state = dir.path().join("state");
    let uid = rustix::process::geteuid().as_raw();

    prepare_vm_state_dir(&state, uid).unwrap();
    assert_eq!(
        std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );

    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    prepare_vm_state_dir(&state, uid).unwrap();
    assert_eq!(
        std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn prepare_vm_state_dir_rejects_symlinks_and_the_wrong_owner() {
    let dir = tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let uid = rustix::process::geteuid().as_raw();

    let error = prepare_vm_state_dir(&link, uid).unwrap_err().to_string();
    assert!(error.contains("symlink"), "{error}");

    let error = prepare_vm_state_dir(&real, uid + 1)
        .unwrap_err()
        .to_string();
    assert!(error.contains("is owned by uid"), "{error}");
}

#[test]
fn driver_dir_comes_from_the_gateway_section_and_the_legacy_key_is_stripped() {
    let from_gateway = VmLaunchPlan::from_table(
        table("driver_dir = \"/legacy\"\nvcpus = 2"),
        Some(Path::new("/gateway")),
    )
    .unwrap();
    assert_eq!(from_gateway.driver_dir, Some(PathBuf::from("/gateway")));
    assert!(!from_gateway.table.contains("driver_dir"));

    let legacy_only = VmLaunchPlan::from_table(table("driver_dir = \"/legacy\""), None).unwrap();
    assert_eq!(legacy_only.driver_dir, Some(PathBuf::from("/legacy")));
    assert!(!legacy_only.table.contains("driver_dir"));

    let gateway_only = VmLaunchPlan::from_table(table(""), Some(Path::new("/gateway"))).unwrap();
    assert_eq!(gateway_only.driver_dir, Some(PathBuf::from("/gateway")));
}

#[test]
fn the_admission_policy_is_not_forwarded_in_the_driver_table() {
    let plan = VmLaunchPlan::from_table(
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
