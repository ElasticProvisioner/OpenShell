// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=PROTOC");
    let proto_root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("../../proto");
    println!(
        "cargo:rerun-if-changed={}",
        proto_root.join("driver_launch.proto").display()
    );

    let mut config = tonic_prost_build::Config::new();
    // A caller-supplied PROTOC is used as-is; otherwise use the vendored one.
    if std::env::var_os("PROTOC").is_none() {
        config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    }
    tonic_prost_build::configure().compile_with_config(
        config,
        &[proto_root.join("driver_launch.proto")],
        &[proto_root],
    )?;
    Ok(())
}
