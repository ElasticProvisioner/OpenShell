// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::sync::atomic::{AtomicBool, Ordering};

use openshell_driver_sdk::launch::{Launch, LaunchInfo};

use openshell_server::config_file::OtlpConfig;

use super::*;

/// Write an executable script by writing and closing a temporary file, then
/// renaming it into place, so it is never executed while open for writing.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let temporary = dir.join(format!("{name}.tmp"));
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
    let path = dir.join(name);
    std::fs::rename(&temporary, &path).unwrap();
    path
}

async fn run(binary: &Path, table: &str, dir: &Path) -> Result<LaunchOutcome> {
    launch(
        LaunchRequest {
            name: "fake",
            binary,
            table,
            socket_dir: &dir.join("run"),
            log_level: "info",
        },
        ConfigureRequest::default().into(),
        Arc::new(|_| {}),
    )
    .await
}

#[tokio::test]
async fn the_table_arrives_on_stdin_and_bootstrap_arguments_on_argv() {
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record");
    let binary = script(
        dir.path(),
        "driver",
        &format!(
            "printf '%s\\n' \"$@\" > '{0}.argv'\ncat > '{0}.stdin'\nprintf '{{\"result\":\"invalid_config\",\"reason\":\"nope\"}}\\n'\nexit 78",
            record.display()
        ),
    );
    let table = "vcpus = 4\n# comment\n";

    let outcome = run(&binary, table, dir.path()).await.unwrap();

    assert!(matches!(outcome, LaunchOutcome::InvalidConfig(reason) if reason == "nope"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("record.stdin")).unwrap(),
        table
    );
    let argv = std::fs::read_to_string(dir.path().join("record.argv")).unwrap();
    let argv: Vec<_> = argv.lines().collect();
    assert_eq!(argv[0], "serve");
    assert_eq!(argv[1], "--bind-socket");
    assert!(argv[2].contains("/run/"), "{}", argv[2]);
    assert!(argv[2].ends_with("/driver.sock"), "{}", argv[2]);
    assert_eq!(argv[3], "--expected-peer-pid");
    assert_eq!(argv[4], std::process::id().to_string());
    assert_eq!(&argv[5..], ["--log-level", "info"]);
    assert_eq!(argv.len(), 7, "no other flags: {argv:?}");
}

#[tokio::test]
async fn exit_codes_map_to_the_documented_results() {
    let dir = tempfile::tempdir().unwrap();
    let report = |kind: &str| format!("printf '{{\"result\":\"{kind}\",\"reason\":\"why\"}}\\n'\n");
    let cases = [
        ("unavailable", report("unavailable") + "exit 69"),
        ("unavailable-bare", "exit 69".to_string()),
        ("invalid", report("invalid_config") + "exit 78"),
        ("invalid-bare", "exit 78".to_string()),
    ];
    for (name, body) in cases {
        let binary = script(dir.path(), name, &body);
        let outcome = run(&binary, "", &dir.path().join(format!("{name}-dir")))
            .await
            .unwrap();
        match (name, outcome) {
            ("unavailable", LaunchOutcome::Unavailable(reason))
            | ("invalid", LaunchOutcome::InvalidConfig(reason)) => assert_eq!(reason, "why"),
            ("unavailable-bare", LaunchOutcome::Unavailable(_))
            | ("invalid-bare", LaunchOutcome::InvalidConfig(_)) => {}
            _ => panic!("{name} mapped to the wrong result"),
        }
    }

    let binary = script(dir.path(), "crash", "exit 1");
    let error = run(&binary, "", &dir.path().join("crash-dir"))
        .await
        .err()
        .expect("an internal failure is an error");
    assert!(error.to_string().contains("failed to start"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
#[allow(unsafe_code)] // `pre_exec` arms the parent-death signal as a driver does.
fn a_driver_outlives_the_thread_that_asked_for_it() {
    use std::os::unix::process::CommandExt as _;

    let mut command = std::process::Command::new("sleep");
    command.arg("30");
    // SAFETY: only an async-signal-safe `prctl` runs between fork and exec.
    unsafe {
        command.pre_exec(|| {
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL)
                .map_err(std::io::Error::from)
        });
    }
    // The kernel ties the signal to the spawning thread. This thread exits at
    // once, as a Tokio blocking-pool thread does after it has been idle.
    let mut child = std::thread::spawn(move || spawn_from_long_lived_thread(command))
        .join()
        .unwrap()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(child.try_wait().unwrap().is_none(), "the driver was killed");
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn a_configured_directory_is_searched_before_the_standard_locations() {
    let custom = PathBuf::from("/opt/openshell-drivers");
    let dirs = driver_search_dirs(Some(&custom));

    assert_eq!(dirs.first(), Some(&custom));
    // A directory with one driver in it does not hide the others.
    let standard = dirs
        .iter()
        .position(|dir| dir == Path::new("/usr/libexec/openshell"))
        .expect("the standard locations are still searched");
    assert!(standard > 0);

    assert!(!driver_search_dirs(None).contains(&custom));
}

#[test]
fn a_driver_does_not_inherit_the_collector_header_credentials() {
    let command = driver_command(Path::new("openshell-driver-fake"));
    let removed: Vec<String> = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();

    for name in [
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
        "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    ] {
        assert!(
            removed.iter().any(|removed| removed == name),
            "{name} is inherited"
        );
    }
}

/// A driver process that does nothing, and the `Started` that holds it.
fn idle_driver(socket: PathBuf) -> Started {
    Started {
        name: "fake".to_string(),
        child: Some(
            std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        ),
        socket,
        handed_off: false,
    }
}

#[tokio::test(start_paused = true)]
async fn a_driver_that_accepts_the_socket_but_never_speaks_does_not_hang_the_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("a.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        // Accept and hold each connection without ever answering.
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _stream = stream;
                std::future::pending::<()>().await;
            });
        }
    });

    let error = finish(
        idle_driver(socket),
        ConfigureRequest::default().into(),
        Arc::new(|_| {}),
    )
    .await
    .err()
    .expect("a silent driver is an error");

    // tonic connects without waiting for the server, so the wait ends at the
    // first call.
    assert!(
        error.to_string().contains("did not answer GetLaunchInfo"),
        "{error}"
    );
}

/// A driver that speaks gRPC but never answers `GetLaunchInfo`.
struct SilentDriver;

#[tonic::async_trait]
impl openshell_driver_sdk::launch::DriverLaunch for SilentDriver {
    async fn get_launch_info(
        &self,
        _request: tonic::Request<GetLaunchInfoRequest>,
    ) -> std::result::Result<
        tonic::Response<openshell_driver_sdk::launch::GetLaunchInfoResponse>,
        tonic::Status,
    > {
        std::future::pending().await
    }

    async fn configure(
        &self,
        _request: tonic::Request<ConfigureRequest>,
    ) -> std::result::Result<
        tonic::Response<openshell_driver_sdk::launch::ConfigureResponse>,
        tonic::Status,
    > {
        Err(tonic::Status::unimplemented("not used"))
    }

    async fn shutdown(
        &self,
        _request: tonic::Request<ShutdownRequest>,
    ) -> std::result::Result<
        tonic::Response<openshell_driver_sdk::launch::ShutdownResponse>,
        tonic::Status,
    > {
        Err(tonic::Status::unimplemented("not used"))
    }
}

#[tokio::test(start_paused = true)]
async fn a_driver_that_never_answers_get_launch_info_does_not_hang_the_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("b.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(openshell_driver_sdk::launch::DriverLaunchServer::new(
                SilentDriver,
            ))
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );

    let error = finish(
        idle_driver(socket),
        ConfigureRequest::default().into(),
        Arc::new(|_| {}),
    )
    .await
    .err()
    .expect("a driver that does not answer is an error");

    assert!(
        error.to_string().contains("did not answer GetLaunchInfo"),
        "{error}"
    );
}

#[test]
fn a_launch_directory_is_removed_unless_it_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("run");

    let dropped = prepare_socket(&base).unwrap();
    let dropped_dir = dropped.socket().parent().unwrap().to_path_buf();
    assert!(dropped_dir.exists());
    drop(dropped);
    assert!(!dropped_dir.exists(), "a dropped launch left its directory");

    let kept = prepare_socket(&base).unwrap();
    let socket = kept.keep();
    assert!(socket.parent().unwrap().exists());
}

#[test]
fn a_launch_that_cannot_spawn_leaves_no_directory_behind() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("run");

    let error = start(&LaunchRequest {
        name: "fake",
        binary: Path::new("/nonexistent/openshell-driver-fake"),
        table: "",
        socket_dir: &base,
        log_level: "info",
    })
    .err()
    .expect("a binary that does not exist cannot start");

    assert!(error.to_string().contains("failed to launch"), "{error}");
    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
}

#[test]
fn binaries_are_found_in_the_configured_directories() {
    let dir = tempfile::tempdir().unwrap();
    script(dir.path(), "openshell-driver-fake", "exit 0");

    let found = resolve_binary(
        "openshell-driver-fake",
        &[dir.path().join("missing"), dir.path().to_path_buf()],
        "install it",
    )
    .unwrap();
    assert_eq!(found, dir.path().join("openshell-driver-fake"));

    let error = resolve_binary(
        "openshell-driver-absent",
        &[dir.path().to_path_buf()],
        "install it",
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("not found") && error.contains("install it"),
        "{error}"
    );
}

#[test]
fn every_launch_gets_its_own_private_socket_directory() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("run");
    std::fs::create_dir(&base).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();

    let first = prepare_socket(&base).unwrap();
    let second = prepare_socket(&base).unwrap();

    assert_ne!(
        first.socket().parent(),
        second.socket().parent(),
        "launches must not share a directory"
    );
    assert_eq!(first.socket().file_name().unwrap(), SOCKET_NAME);
    for launch_dir in [
        first.socket().parent().unwrap(),
        second.socket().parent().unwrap(),
    ] {
        assert!(launch_dir.starts_with(&base));
        assert_eq!(
            std::fs::metadata(launch_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(
        std::fs::metadata(&base).unwrap().permissions().mode() & 0o777,
        0o700
    );

    // Another launch's socket is never touched.
    std::fs::write(first.socket(), b"first").unwrap();
    prepare_socket(&base).unwrap();
    assert_eq!(std::fs::read(first.socket()).unwrap(), b"first");
}

#[tokio::test]
async fn a_launch_that_fails_removes_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let binary = script(dir.path(), "driver", "exit 78");
    let base = dir.path().join("base");

    run(&binary, "", &base).await.unwrap();

    let leftovers = std::fs::read_dir(base.join("run")).map_or(0, Iterator::count);
    assert_eq!(
        leftovers, 0,
        "the failed launch's directory must be removed"
    );
}

#[tokio::test]
async fn the_exit_status_decides_the_result_not_the_report() {
    let dir = tempfile::tempdir().unwrap();
    // A report that claims "unavailable" cannot turn a crash into an absent
    // backend, which would silently fall through to another driver.
    let lies = script(
        dir.path(),
        "lies",
        "printf '{\"result\":\"unavailable\",\"reason\":\"nope\"}\\n'\nexit 1",
    );
    let error = run(&lies, "", &dir.path().join("lies-dir"))
        .await
        .err()
        .expect("an exit status of 1 is an internal failure");
    assert!(error.to_string().contains("nope"), "{error}");

    let killed = script(dir.path(), "killed", "kill -9 $$");
    assert!(
        run(&killed, "", &dir.path().join("killed-dir"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_unexpected_exit_is_reported_and_a_released_process_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = |script: &str| {
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };

    let reported = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&reported);
    let exits = Arc::new(ManagedDriverProcess::new(
        spawn("exit 7"),
        dir.path().join("a"),
    ));
    watch_process(
        "fake".into(),
        &exits,
        Arc::new(move |message| {
            assert!(message.contains("fake"), "{message}");
            flag.store(true, Ordering::SeqCst);
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reported.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the exit was never reported");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let spurious = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&spurious);
    let released = Arc::new(ManagedDriverProcess::new(
        spawn("sleep 30"),
        dir.path().join("b"),
    ));
    watch_process(
        "fake".into(),
        &released,
        Arc::new(move |_| flag.store(true, Ordering::SeqCst)),
    );
    drop(released);
    tokio::time::sleep(EXIT_WATCH_INTERVAL * 2 + Duration::from_millis(200)).await;
    assert!(!spurious.load(Ordering::SeqCst));
}

async fn serve_launch(
    socket: &Path,
    name: &str,
    seen: Arc<Mutex<Option<ConfigureRequest>>>,
) -> Channel {
    let launch = Launch::new(
        LaunchInfo {
            driver_name: name.to_string(),
            driver_version: "1.0".to_string(),
            features: Vec::new(),
        },
        move |request| {
            let seen = Arc::clone(&seen);
            async move {
                *seen.lock().unwrap() = Some(request);
                Ok(())
            }
        },
    );
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(launch.into_server())
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );
    connect(socket).await.unwrap()
}

#[tokio::test]
async fn the_handshake_configures_the_driver_it_expects() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(None));
    let channel = serve_launch(&dir.path().join("a.sock"), "fake", Arc::clone(&seen)).await;
    let configure = ConfigureRequest {
        gateway: Some(openshell_driver_sdk::launch::GatewayContext {
            gateway_port: 8080,
            ..Default::default()
        }),
        ..Default::default()
    };

    handshake(&channel, "fake", configure.into()).await.unwrap();

    let received = seen.lock().unwrap().take().expect("configured");
    assert_eq!(received.protocol_version, PROTOCOL_VERSION);
    assert_eq!(received.gateway.unwrap().gateway_port, 8080);

    let seen = Arc::new(Mutex::new(None));
    let channel = serve_launch(&dir.path().join("b.sock"), "other", Arc::clone(&seen)).await;
    let error = handshake(&channel, "fake", ConfigureRequest::default().into())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("'other'"), "{error}");
    assert!(
        seen.lock().unwrap().is_none(),
        "a mismatched driver is not configured"
    );
}

#[test]
fn preflight_maps_its_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let stdin = dir.path().join("stdin");
    let accepts = script(
        dir.path(),
        "accepts",
        &format!("cat > '{}'", stdin.display()),
    );
    preflight(&accepts, "vcpus = 4\n").unwrap();
    assert_eq!(std::fs::read_to_string(stdin).unwrap(), "vcpus = 4\n");

    let rejects = script(
        dir.path(),
        "rejects",
        "printf '{\"result\":\"invalid_config\",\"reason\":\"unknown key\"}\\n'\nexit 78",
    );
    assert_eq!(
        preflight(&rejects, ""),
        Err(PreflightFailure::InvalidConfig("unknown key".to_string()))
    );

    let breaks = script(dir.path(), "breaks", "exit 3");
    assert!(matches!(
        preflight(&breaks, ""),
        Err(PreflightFailure::Error(_))
    ));
}

#[test]
fn configure_carries_the_gateway_metadata_the_argv_used_to() {
    let ca_dir = tempfile::tempdir().unwrap();
    let ca = ca_dir.path().join("collector-ca.pem");
    let pki = openshell_otel_test_support::TestPki::generate();
    std::fs::write(&ca, &pki.ca_pem).unwrap();
    let otlp: OtlpConfig = toml::from_str(&format!(
        r#"
        [traces]
        endpoint = "https://traces.test:4317"
        protocol = "http/protobuf"
        ca_file = "{}"
        [ocsf]
        enabled = true
        endpoint = "https://ocsf.test:4317"
        "#,
        ca.display()
    ))
    .unwrap();
    let bundle = openshell_core::NetworkSupervisorTrustBundle::new(
        b"pem".to_vec(),
        2,
        "sha256:abc",
        PathBuf::from("/var/lib/openshell/network/additional-ca.crt"),
    );
    let admission = openshell_core::resource_admission::DriverAdmissionConfig {
        allow_driver_config: true,
        ..Default::default()
    };
    let inputs = ConfigureInputs {
        gateway_bind_address: "127.0.0.1:8080".parse().unwrap(),
        guest_tls_ca: Some(Path::new("/etc/openshell/ca.pem")),
        gateway_name: "gw",
        network_trust: Some(&bundle),
        otlp: Some(&otlp),
        admission: Some(&admission),
        sandbox_runtime_image: Some("runtime:1".to_string()),
        supervisor_image: None,
        configure_timeout: DEFAULT_CONFIGURE_TIMEOUT,
    };

    let request = inputs.request();

    assert_eq!(request.protocol_version, PROTOCOL_VERSION);
    let gateway = request.gateway.unwrap();
    assert_eq!(gateway.gateway_port, 8080);
    assert_eq!(gateway.bind_address, "127.0.0.1:8080");
    assert_eq!(gateway.tls_ca_path, "/etc/openshell/ca.pem");
    assert_eq!(gateway.gateway_name, "gw");
    let trust = request.network_trust.unwrap();
    assert_eq!(
        trust.artifact_path,
        "/var/lib/openshell/network/additional-ca.crt"
    );
    assert_eq!(trust.digest, "sha256:abc");
    assert_eq!(trust.certificate_count, 2);
    let images = request.image_overrides.unwrap();
    assert_eq!(images.sandbox_runtime_image.as_deref(), Some("runtime:1"));
    assert_eq!(images.supervisor_image, None);
    let telemetry = request.telemetry.unwrap();
    let traces = telemetry.traces.unwrap();
    assert_eq!(traces.endpoint, "https://traces.test:4317");
    assert_eq!(traces.protocol, "http/protobuf");
    assert_eq!(telemetry.ocsf.unwrap().endpoint, "https://ocsf.test:4317");
    assert!(telemetry.agent.is_none());
    let sent: openshell_core::resource_admission::DriverAdmissionConfig =
        serde_json::from_str(&request.resource_admission_policy_json).unwrap();
    assert!(sent.allow_driver_config);
    assert_eq!(
        telemetry.collector.unwrap().ca_file,
        ca.display().to_string()
    );
}

#[test]
fn configure_omits_what_the_gateway_does_not_have() {
    let inputs = ConfigureInputs {
        gateway_bind_address: "127.0.0.1:8080".parse().unwrap(),
        guest_tls_ca: None,
        gateway_name: "gw",
        network_trust: None,
        otlp: None,
        admission: None,
        sandbox_runtime_image: None,
        supervisor_image: None,
        configure_timeout: DEFAULT_CONFIGURE_TIMEOUT,
    };

    let request = inputs.request();

    assert_eq!(request.gateway.unwrap().tls_ca_path, "");
    assert!(request.network_trust.is_none());
    assert!(request.telemetry.is_none());
    assert!(request.resource_admission_policy_json.is_empty());
}

#[tokio::test]
#[cfg(any(feature = "compute-driver-docker", feature = "compute-driver-podman"))]
async fn an_unreachable_backend_is_retried_a_bounded_number_of_times() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("starts");
    let binary = script(
        dir.path(),
        "driver",
        &format!("echo x >> '{}'\nexit 69", counter.display()),
    );

    let outcome = launch_tolerating_unavailable(
        LaunchRequest {
            name: "fake",
            binary: &binary,
            table: "",
            socket_dir: &dir.path().join("run"),
            log_level: "info",
        },
        ConfigureRequest::default().into(),
        Arc::new(|_| {}),
        UnavailableRetry {
            attempts: 3,
            delay: Duration::from_millis(5),
        },
    )
    .await
    .unwrap();

    assert!(matches!(outcome, LaunchOutcome::Unavailable(_)));
    assert_eq!(std::fs::read_to_string(counter).unwrap().lines().count(), 3);
}

#[tokio::test]
#[cfg(any(feature = "compute-driver-docker", feature = "compute-driver-podman"))]
async fn an_invalid_table_or_a_crash_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    for (name, code) in [("invalid", 78), ("crash", 1)] {
        let counter = dir.path().join(format!("{name}-starts"));
        let binary = script(
            dir.path(),
            name,
            &format!("echo x >> '{}'\nexit {code}", counter.display()),
        );
        let _ = launch_tolerating_unavailable(
            LaunchRequest {
                name: "fake",
                binary: &binary,
                table: "",
                socket_dir: &dir.path().join(format!("{name}-run")),
                log_level: "info",
            },
            ConfigureRequest::default().into(),
            Arc::new(|_| {}),
            UnavailableRetry {
                attempts: 3,
                delay: Duration::from_millis(5),
            },
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(counter).unwrap().lines().count(),
            1,
            "{name} must not be retried"
        );
    }
}

#[test]
fn the_configure_call_carries_the_gateways_budget() {
    let inputs = ConfigureInputs::from_environment(
        "127.0.0.1:8080".parse().unwrap(),
        None,
        "gw",
        None,
        None,
        None,
    );
    assert_eq!(inputs.configure_call().timeout, DEFAULT_CONFIGURE_TIMEOUT);

    let inputs = inputs.with_configure_timeout(Duration::from_mins(45));
    assert_eq!(inputs.configure_call().timeout, Duration::from_mins(45));
}
