// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Launch a compute driver as a subprocess.
//!
//! The gateway starts `<driver> serve` with bootstrap arguments only and the
//! operator's driver table on stdin. When the driver accepts a connection, the
//! gateway reads `GetLaunchInfo` and sends gateway-owned metadata in
//! `Configure`. The table itself is never parsed here beyond removing the
//! gateway-owned keys the caller already took out.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyper_util::rt::TokioIo;
use openshell_core::{Error, Result};
use openshell_driver_sdk::launch::proto::driver_launch_client::DriverLaunchClient;
use openshell_driver_sdk::launch::{
    ConfigureRequest, GetLaunchInfoRequest, PROTOCOL_VERSION, ShutdownRequest,
};
use openshell_driver_sdk::{StartKind, StartReport};
use openshell_server::{DriverProcessState, GracefulStop, ManagedDriverProcess};
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

const SOCKET_NAME: &str = "driver.sock";
/// How long a driver has to accept a connection after it starts.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a driver has to answer `Configure` when the caller sets no budget.
/// Constructing the driver can pull its supervisor and runtime images.
pub const DEFAULT_CONFIGURE_TIMEOUT: Duration = Duration::from_mins(30);
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a driver has to accept the gRPC connection and answer
/// `GetLaunchInfo`. Neither does any work, so this only bounds a driver that
/// accepts its socket and then does not answer, which `START_TIMEOUT` and the
/// `Configure` deadline do not cover.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const EXIT_WATCH_INTERVAL: Duration = Duration::from_secs(1);

pub fn default_driver_search_dirs(home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.push(home.join(".local").join("libexec").join("openshell"));
    }
    for dir in [
        "/usr/libexec/openshell",
        "/usr/local/libexec/openshell",
        "/usr/local/libexec",
    ] {
        push_unique_path(&mut dirs, PathBuf::from(dir));
    }
    dirs
}

/// The directories to search for a driver binary: `driver_dir` first when it
/// is set, then the standard locations.
///
/// `driver_dir` applies to every driver, so it must not hide the drivers that
/// live elsewhere: a directory that holds only the VM driver still leaves the
/// Docker and Podman drivers findable in the standard locations.
pub fn driver_search_dirs(driver_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = driver_dir {
        push_unique_path(&mut dirs, dir.to_path_buf());
    }
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(prefix) = current_exe.parent().and_then(Path::parent)
    {
        push_unique_path(&mut dirs, prefix.join("libexec"));
        push_unique_path(&mut dirs, prefix.join("libexec").join("openshell"));
    }
    for dir in default_driver_search_dirs(std::env::var_os("HOME").map(PathBuf::from)) {
        push_unique_path(&mut dirs, dir);
    }
    dirs
}

/// Variables a driver does not inherit from the gateway. They carry collector
/// request headers, which can hold credentials; the gateway resolves the trace
/// headers and sends them in `Configure`.
const BLOCKED_ENV_VARS: [&str; 4] = [
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
];

/// A command that runs a driver, without the variables in
/// [`BLOCKED_ENV_VARS`]. `preflight` never needs them either.
fn driver_command(binary: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(binary);
    for name in BLOCKED_ENV_VARS {
        command.env_remove(name);
    }
    command
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

/// Search `dirs` for `bin_name`, then next to the gateway executable.
pub fn resolve_binary(bin_name: &str, dirs: &[PathBuf], hint: &str) -> Result<PathBuf> {
    let mut searched = Vec::new();
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|parent| parent.join(bin_name)));
    for candidate in dirs.iter().map(|dir| dir.join(bin_name)).chain(sibling) {
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !searched.contains(&candidate) {
            searched.push(candidate);
        }
    }
    let searched = searched
        .iter()
        .map(|path| format!("'{}'", path.display()))
        .collect::<Vec<_>>()
        .join(", ");
    Err(Error::config(format!(
        "compute driver binary {bin_name} not found (searched {searched}); {hint}"
    )))
}

/// A launch's private directory.
///
/// It is removed when this drops, until [`LaunchDir::keep`] hands it to the
/// handle that owns the launch, so a launch that fails before then leaves
/// nothing behind.
pub struct LaunchDir {
    dir: Option<tempfile::TempDir>,
    socket: PathBuf,
}

impl LaunchDir {
    /// Where the driver binds its socket.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Stop removing the directory, and return the socket path in it.
    #[must_use]
    pub fn keep(mut self) -> PathBuf {
        if let Some(dir) = self.dir.take() {
            let _ = dir.keep();
        }
        std::mem::take(&mut self.socket)
    }
}

/// Create a private directory for one launch's socket under `base`.
///
/// Every launch gets its own directory, so a launch only configures and
/// cleans up its own child: a second startup under the same account cannot
/// remove a running driver's socket. The directory is removed with the
/// driver's handle once the launch owns it, and by the returned guard until
/// then.
pub fn prepare_socket(base: &Path) -> Result<LaunchDir> {
    let euid = rustix::process::geteuid().as_raw();
    std::fs::create_dir_all(base).map_err(|error| {
        Error::execution(format!(
            "failed to create compute driver socket dir '{}': {error}",
            base.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(base).map_err(|error| {
        Error::execution(format!(
            "failed to stat socket dir '{}': {error}",
            base.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(Error::execution(format!(
            "compute driver socket dir '{}' is not a plain directory",
            base.display()
        )));
    }
    if metadata.uid() != euid {
        return Err(Error::execution(format!(
            "compute driver socket dir '{}' is owned by uid {} but the gateway runs as {euid}",
            base.display(),
            metadata.uid()
        )));
    }
    std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        Error::execution(format!(
            "failed to restrict socket dir '{}': {error}",
            base.display()
        ))
    })?;
    let dir = tempfile::Builder::new()
        .prefix("")
        .tempdir_in(base)
        .map_err(|error| {
            Error::execution(format!(
                "failed to create a launch directory in '{}': {error}",
                base.display()
            ))
        })?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).map_err(
        |error| {
            Error::execution(format!(
                "failed to restrict launch directory '{}': {error}",
                dir.path().display()
            ))
        },
    )?;
    let socket = dir.path().join(SOCKET_NAME);
    Ok(LaunchDir {
        dir: Some(dir),
        socket,
    })
}

/// The driver table as a driver's stdin.
///
/// This is an unlinked, private temporary file, not a pipe. A pipe's end of
/// file waits for every copy of its write end to close, and on macOS a child
/// spawned by another thread can briefly inherit one. A file ends where the
/// table ends.
fn table_stdin(table: &str) -> std::io::Result<Stdio> {
    use std::io::{Seek as _, SeekFrom, Write as _};

    let mut file = tempfile::tempfile()?;
    file.write_all(table.as_bytes())?;
    file.seek(SeekFrom::Start(0))?;
    Ok(Stdio::from(file))
}

/// The stdout of a driver, forwarded line by line, with its start report kept.
/// Stderr is the driver's own: it passes straight through to the gateway's.
#[derive(Clone, Default)]
struct StdoutCapture {
    report: Arc<Mutex<Option<StartReport>>>,
}

impl StdoutCapture {
    /// Forward `stdout` until the driver closes it, keeping the start report.
    fn spawn(&self, stdout: std::process::ChildStdout) -> std::thread::JoinHandle<()> {
        use std::io::BufRead as _;

        let report = Arc::clone(&self.report);
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if let Ok(parsed) = serde_json::from_str::<StartReport>(line.trim()) {
                    *report
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(parsed);
                } else {
                    println!("{line}");
                }
            }
        })
    }

    fn take(&self) -> Option<StartReport> {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// How a driver that exited before serving is described.
fn exit_outcome(
    name: &str,
    code: Option<i32>,
    status: std::process::ExitStatus,
    report: Option<StartReport>,
) -> Result<StartOutcome> {
    // The exit status decides the result; the report only explains it. A
    // crash or a signal is never an absent backend.
    let kind = code.map_or(StartKind::Error, StartKind::from_exit_code);
    let reason = report.map_or_else(|| status.to_string(), |report| report.reason);
    match kind {
        StartKind::Unavailable => Ok(StartOutcome::Unavailable(reason)),
        StartKind::InvalidConfig => Ok(StartOutcome::InvalidConfig(reason)),
        StartKind::Error => Err(Error::execution(format!(
            "compute driver '{name}' failed to start: {reason}"
        ))),
    }
}

/// A driver process that has started and accepts connections but is not yet
/// configured. Dropping it stops the process.
pub struct Started {
    name: String,
    child: Option<std::process::Child>,
    socket: PathBuf,
    /// `finish` handed the process and its launch directory to a managed
    /// process.
    handed_off: bool,
}

impl Drop for Started {
    fn drop(&mut self) {
        if self.handed_off {
            return;
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // The launch directory belongs to this launch.
        let _ = std::fs::remove_file(&self.socket);
        if let Some(dir) = self.socket.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// How starting a driver ended.
pub enum StartOutcome {
    /// The driver is running and accepts connections.
    Started(Started),
    /// The driver's backend is not reachable with these settings.
    Unavailable(String),
    /// The driver rejected its table.
    InvalidConfig(String),
}

/// How a launch ended before the driver was serving.
pub enum LaunchOutcome {
    /// The driver is running and configured.
    Launched(Launched),
    /// The driver's backend is not reachable with these settings.
    Unavailable(String),
    /// The driver rejected its table.
    InvalidConfig(String),
}

/// A running, configured driver.
pub struct Launched {
    pub channel: Channel,
    pub process: Arc<ManagedDriverProcess>,
}

/// What to launch.
#[derive(Clone, Copy)]
pub struct LaunchRequest<'a> {
    /// The driver name `GetLaunchInfo` must report, such as `vm`.
    pub name: &'a str,
    pub binary: &'a Path,
    /// The driver table as TOML text.
    pub table: &'a str,
    /// A directory the gateway owns, made private (`0700`) for the socket.
    pub socket_dir: &'a Path,
    pub log_level: &'a str,
}

/// Spawn `command` from a thread that lives as long as the gateway.
///
/// A driver may arm `PR_SET_PDEATHSIG`, which the kernel ties to the thread
/// that spawned it, not to the process. `start` runs on a Tokio blocking-pool
/// thread, which exits after about ten idle seconds and would take the driver
/// with it.
fn spawn_from_long_lived_thread(
    command: std::process::Command,
) -> std::io::Result<std::process::Child> {
    use std::sync::{OnceLock, PoisonError, mpsc};

    type Reply = mpsc::Sender<std::io::Result<std::process::Child>>;
    static SPAWNER: OnceLock<Mutex<mpsc::Sender<(std::process::Command, Reply)>>> = OnceLock::new();

    let spawner = SPAWNER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<(std::process::Command, Reply)>();
        std::thread::Builder::new()
            .name("driver-spawner".to_string())
            .spawn(move || {
                for (mut command, reply) in receiver {
                    let _ = reply.send(command.spawn());
                }
            })
            .expect("start the driver spawner thread");
        Mutex::new(sender)
    });
    let (reply, result) = mpsc::channel();
    spawner
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .send((command, reply))
        .map_err(|_| std::io::Error::other("the driver spawner stopped"))?;
    result
        .recv()
        .map_err(|_| std::io::Error::other("the driver spawner stopped"))?
}

/// Start `<driver> serve` with `request.table` on stdin and wait until it
/// accepts a connection or exits. Blocking: callers in async code use
/// `spawn_blocking`.
///
/// Detection and launch share this. The driver checks its table and backend
/// before it binds, so a driver that accepts a connection is available, and
/// the same process goes on to be configured.
pub fn start(request: &LaunchRequest<'_>) -> Result<StartOutcome> {
    let name = request.name;
    let launch_dir = prepare_socket(request.socket_dir)?;
    let socket = launch_dir.socket().to_path_buf();
    let stdin = table_stdin(request.table)
        .map_err(|error| Error::execution(format!("failed to stage the driver table: {error}")))?;
    let mut command = driver_command(request.binary);
    command
        .arg("serve")
        .arg("--bind-socket")
        .arg(&socket)
        .arg("--expected-peer-pid")
        .arg(std::process::id().to_string())
        .arg("--log-level")
        .arg(request.log_level)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = spawn_from_long_lived_thread(command).map_err(|error| {
        Error::execution(format!(
            "failed to launch compute driver '{}': {error}",
            request.binary.display()
        ))
    })?;
    let capture = StdoutCapture::default();
    let stdout_task = capture.spawn(child.stdout.take().expect("stdout is piped"));
    let mut started = Started {
        name: name.to_string(),
        child: Some(child),
        // From here the handle owns the directory.
        socket: launch_dir.keep(),
        handed_off: false,
    };

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let child = started
            .child
            .as_mut()
            .expect("the child is held until it exits");
        if let Some(status) = child.try_wait().map_err(|error| {
            Error::execution(format!("failed to poll compute driver '{name}': {error}"))
        })? {
            started.child = None;
            // The driver's end of stdout is closed; let the reader finish.
            let wait_until = Instant::now() + Duration::from_secs(1);
            while !stdout_task.is_finished() && Instant::now() < wait_until {
                std::thread::sleep(Duration::from_millis(10));
            }
            return exit_outcome(name, status.code(), status, capture.take());
        }
        if std::os::unix::net::UnixStream::connect(&started.socket).is_ok() {
            return Ok(StartOutcome::Started(started));
        }
        if Instant::now() >= deadline {
            return Err(Error::execution(format!(
                "timed out waiting for compute driver '{name}' to accept connections on '{}'",
                started.socket.display()
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// A `Configure` call: the request and how long the driver has to answer it.
///
/// Constructing a driver can pull its images, so the deadline is the
/// gateway's image preparation budget, not a fixed short time.
#[derive(Clone)]
pub struct ConfigureCall {
    /// The gateway-owned values to deliver. The protocol version is filled in
    /// when the request is sent.
    pub request: ConfigureRequest,
    /// How long the driver has to answer.
    pub timeout: Duration,
}

impl From<ConfigureRequest> for ConfigureCall {
    fn from(request: ConfigureRequest) -> Self {
        Self {
            request,
            timeout: DEFAULT_CONFIGURE_TIMEOUT,
        }
    }
}

/// Configure a driver that `start` returned and begin watching its process.
pub async fn finish(
    mut started: Started,
    configure: ConfigureCall,
    on_unexpected_exit: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<Launched> {
    let channel = tokio::time::timeout(HANDSHAKE_TIMEOUT, connect(&started.socket))
        .await
        .map_err(|_| {
            Error::execution(format!(
                "compute driver '{}' did not accept a gRPC connection within {} seconds",
                started.name,
                HANDSHAKE_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| Error::execution(format!("failed to connect to driver: {error}")))?;
    handshake(&channel, &started.name, configure).await?;
    let child = started
        .child
        .take()
        .expect("a started driver holds its process");
    started.handed_off = true;
    // Release the driver through its own protocol before the process is
    // signalled, so it finishes the calls in flight first.
    let release_channel = channel.clone();
    let process = Arc::new(
        ManagedDriverProcess::from_std(child, started.socket.clone()).with_graceful_stop(
            GracefulStop::new(move || {
                let channel = release_channel.clone();
                async move {
                    DriverLaunchClient::new(channel)
                        .shutdown(ShutdownRequest {})
                        .await
                        .is_ok()
                }
            }),
        ),
    );
    watch_process(started.name.clone(), &process, on_unexpected_exit);
    Ok(Launched { channel, process })
}

/// Start a driver and configure it.
pub async fn launch(
    request: LaunchRequest<'_>,
    configure: ConfigureCall,
    on_unexpected_exit: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<LaunchOutcome> {
    let owned = (
        request.name.to_string(),
        request.binary.to_path_buf(),
        request.table.to_string(),
        request.socket_dir.to_path_buf(),
        request.log_level.to_string(),
    );
    let outcome = tokio::task::spawn_blocking(move || {
        start(&LaunchRequest {
            name: &owned.0,
            binary: &owned.1,
            table: &owned.2,
            socket_dir: &owned.3,
            log_level: &owned.4,
        })
    })
    .await
    .map_err(|error| Error::execution(format!("driver start task failed: {error}")))??;
    match outcome {
        StartOutcome::Started(started) => Ok(LaunchOutcome::Launched(
            finish(started, configure, on_unexpected_exit).await?,
        )),
        StartOutcome::Unavailable(reason) => Ok(LaunchOutcome::Unavailable(reason)),
        StartOutcome::InvalidConfig(reason) => Ok(LaunchOutcome::InvalidConfig(reason)),
    }
}

/// How often to start a driver whose backend is unreachable.
#[cfg(any(feature = "compute-driver-docker", feature = "compute-driver-podman"))]
#[derive(Clone, Copy)]
pub struct UnavailableRetry {
    pub attempts: u32,
    pub delay: Duration,
}

/// [`launch`], starting the driver again while its backend is unreachable.
///
/// Only an "unavailable" result is retried. An invalid table or a crash is
/// final.
#[cfg(any(feature = "compute-driver-docker", feature = "compute-driver-podman"))]
pub async fn launch_tolerating_unavailable(
    request: LaunchRequest<'_>,
    configure: ConfigureCall,
    on_unexpected_exit: Arc<dyn Fn(String) + Send + Sync>,
    retry: UnavailableRetry,
) -> Result<LaunchOutcome> {
    let attempts = retry.attempts.max(1);
    let mut attempt = 1;
    loop {
        let outcome = launch(request, configure.clone(), Arc::clone(&on_unexpected_exit)).await?;
        match outcome {
            LaunchOutcome::Unavailable(reason) if attempt < attempts => {
                tracing::warn!(
                    driver = request.name,
                    attempt,
                    attempts,
                    %reason,
                    "compute driver backend is not reachable yet; retrying"
                );
                tokio::time::sleep(retry.delay).await;
                attempt += 1;
            }
            outcome => return Ok(outcome),
        }
    }
}

/// Check the driver's identity and protocol, then configure it.
pub async fn handshake(channel: &Channel, name: &str, configure: ConfigureCall) -> Result<()> {
    let mut client = DriverLaunchClient::new(channel.clone());
    let info = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        client.get_launch_info(GetLaunchInfoRequest {}),
    )
    .await
    .map_err(|_| {
        Error::execution(format!(
            "compute driver '{name}' did not answer GetLaunchInfo within {} seconds",
            HANDSHAKE_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|status| Error::execution(format!("GetLaunchInfo failed: {status}")))?
    .into_inner();
    if info.driver_name != name {
        return Err(Error::execution(format!(
            "expected compute driver '{name}' but the process reports '{}'",
            info.driver_name
        )));
    }
    if !info.protocol_versions.contains(&PROTOCOL_VERSION) {
        return Err(Error::execution(format!(
            "compute driver '{name}' {} supports launch protocol versions {:?}, not {PROTOCOL_VERSION}; upgrade the gateway and driver together",
            info.driver_version, info.protocol_versions
        )));
    }
    let mut request = tonic::Request::new(ConfigureRequest {
        protocol_version: PROTOCOL_VERSION,
        ..configure.request
    });
    request.set_timeout(configure.timeout);
    client.configure(request).await.map_err(|status| {
        Error::execution(format!("configuring compute driver '{name}': {status}"))
    })?;
    Ok(())
}

/// Report a managed driver's unexpected exit once. A stop the gateway asked
/// for, which releases the process, is not reported.
fn watch_process(
    name: String,
    process: &Arc<ManagedDriverProcess>,
    on_unexpected_exit: Arc<dyn Fn(String) + Send + Sync>,
) {
    let process = Arc::downgrade(process);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(EXIT_WATCH_INTERVAL).await;
            let Some(process) = process.upgrade() else {
                return;
            };
            match process.poll_exit() {
                DriverProcessState::Running => {}
                DriverProcessState::Released => return,
                DriverProcessState::Exited(how) => {
                    on_unexpected_exit(format!("compute driver '{name}' {how}"));
                    return;
                }
            }
        }
    });
}

/// The default response to a driver that exited unexpectedly: log it and exit
/// nonzero, which leaves the sandboxes running.
pub fn exit_gateway_on_driver_exit() -> Arc<dyn Fn(String) + Send + Sync> {
    Arc::new(|message| {
        tracing::error!("{message}; the gateway cannot serve without it and is exiting");
        std::process::exit(1);
    })
}

async fn connect(socket: &Path) -> std::result::Result<Channel, tonic::transport::Error> {
    let socket = socket.to_path_buf();
    Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(service_fn(move |_: tonic::transport::Uri| {
            let socket = socket.clone();
            async move { UnixStream::connect(socket).await.map(TokioIo::new) }
        }))
        .await
}

/// A driver's answer to `preflight`.
#[derive(Debug, PartialEq, Eq)]
pub enum PreflightFailure {
    /// The driver rejected the table.
    InvalidConfig(String),
    /// The check itself failed.
    Error(String),
}

/// Run `<driver> preflight` with the table on stdin. The driver contacts no
/// backend and creates no state.
pub fn preflight(binary: &Path, table: &str) -> std::result::Result<(), PreflightFailure> {
    use std::io::Read as _;

    let mut child = driver_command(binary)
        .arg("preflight")
        .stdin(table_stdin(table).map_err(|error| {
            PreflightFailure::Error(format!("failed to stage the driver table: {error}"))
        })?)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| {
            PreflightFailure::Error(format!("failed to run '{}': {error}", binary.display()))
        })?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });

    let deadline = Instant::now() + PREFLIGHT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PreflightFailure::Error(format!(
                    "'{}' preflight timed out",
                    binary.display()
                )));
            }
            Err(error) => return Err(PreflightFailure::Error(error.to_string())),
        }
    };
    let stdout = reader.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let report = StartReport::from_output(&stdout);
    let reason = report
        .as_ref()
        .map_or_else(|| status.to_string(), |report| report.reason.clone());
    // The exit status decides the result; the report only explains it.
    match status.code().map(StartKind::from_exit_code) {
        Some(StartKind::InvalidConfig) => Err(PreflightFailure::InvalidConfig(reason)),
        _ => Err(PreflightFailure::Error(reason)),
    }
}

/// Gateway-owned values a driver receives in `Configure`.
pub struct ConfigureInputs<'a> {
    /// The gateway's listener. Its port is the gateway port.
    pub gateway_bind_address: std::net::SocketAddr,
    /// The gateway CA guests trust; unset when the gateway serves plaintext.
    pub guest_tls_ca: Option<&'a Path>,
    pub gateway_name: &'a str,
    pub network_trust: Option<&'a openshell_core::NetworkSupervisorTrustBundle>,
    pub otlp: Option<&'a openshell_server::config_file::OtlpConfig>,
    /// The operator's admission policy, which the gateway owns.
    pub admission: Option<&'a openshell_core::resource_admission::DriverAdmissionConfig>,
    pub sandbox_runtime_image: Option<String>,
    pub supervisor_image: Option<String>,
    /// How long the driver has to answer `Configure`.
    pub configure_timeout: Duration,
}

impl<'a> ConfigureInputs<'a> {
    /// The inputs with image overrides read from the gateway's environment.
    #[must_use]
    pub fn from_environment(
        gateway_bind_address: std::net::SocketAddr,
        guest_tls_ca: Option<&'a Path>,
        gateway_name: &'a str,
        network_trust: Option<&'a openshell_core::NetworkSupervisorTrustBundle>,
        otlp: Option<&'a openshell_server::config_file::OtlpConfig>,
        admission: Option<&'a openshell_core::resource_admission::DriverAdmissionConfig>,
    ) -> Self {
        Self {
            gateway_bind_address,
            guest_tls_ca,
            gateway_name,
            network_trust,
            otlp,
            admission,
            sandbox_runtime_image: std::env::var(openshell_core::config::SANDBOX_RUNTIME_IMAGE_ENV)
                .ok(),
            supervisor_image: std::env::var(openshell_core::config::SUPERVISOR_IMAGE_ENV).ok(),
            configure_timeout: DEFAULT_CONFIGURE_TIMEOUT,
        }
    }

    /// Give the driver `timeout` to answer `Configure`. Constructing a driver
    /// can pull its images, so this is the gateway's image preparation budget.
    #[must_use]
    pub fn with_configure_timeout(mut self, timeout: Duration) -> Self {
        self.configure_timeout = timeout;
        self
    }

    /// The `Configure` call for these inputs: the request and its deadline.
    #[must_use]
    pub fn configure_call(&self) -> ConfigureCall {
        ConfigureCall {
            request: self.request(),
            timeout: self.configure_timeout,
        }
    }

    /// The `Configure` request for these inputs. The protocol version is
    /// filled in when the request is sent.
    #[must_use]
    pub fn request(&self) -> ConfigureRequest {
        use openshell_driver_sdk::launch as proto;

        let export = |export: Option<&openshell_core::sandbox_env::OtlpExport>| {
            export.map(|export| proto::OtlpExport {
                endpoint: export.endpoint.clone(),
                protocol: export.protocol.as_str().to_string(),
            })
        };
        let supervisor = self
            .otlp
            .map(openshell_server::config_file::OtlpConfig::supervisor_otlp)
            .unwrap_or_default();
        let collector = self
            .otlp
            .and_then(|otlp| otlp.traces.as_ref())
            .map(|traces| &traces.collector);
        let path = |path: Option<&Path>| {
            path.map(|path| path.display().to_string())
                .unwrap_or_default()
        };
        let telemetry = (!supervisor.is_empty()).then(|| proto::Telemetry {
            traces: export(supervisor.traces.as_ref()),
            ocsf: export(supervisor.ocsf.as_ref()),
            agent: export(supervisor.agent.as_ref()),
            collector: collector.map(|collector| proto::CollectorTls {
                ca_file: path(collector.ca_file()),
                cert_file: path(collector.cert_file()),
                key_file: path(collector.key_file()),
            }),
            // Secret: it travels only here, over the private socket.
            trace_headers: openshell_server::gateway_trace_headers(self.otlp)
                .map(|headers| headers.expose_secret().to_string())
                .unwrap_or_default(),
        });
        ConfigureRequest {
            protocol_version: PROTOCOL_VERSION,
            required_features: Vec::new(),
            gateway: Some(proto::GatewayContext {
                gateway_port: u32::from(self.gateway_bind_address.port()),
                bind_address: self.gateway_bind_address.to_string(),
                tls_ca_path: path(self.guest_tls_ca),
                gateway_name: self.gateway_name.to_string(),
            }),
            network_trust: self.network_trust.map(|bundle| proto::NetworkTrust {
                artifact_path: bundle.artifact_path().display().to_string(),
                digest: bundle.digest().to_string(),
                certificate_count: u32::try_from(bundle.certificate_count()).unwrap_or(u32::MAX),
            }),
            image_overrides: Some(proto::ImageOverrides {
                sandbox_runtime_image: self.sandbox_runtime_image.clone(),
                supervisor_image: self.supervisor_image.clone(),
            }),
            telemetry,
            resource_admission_policy_json: self
                .admission
                .and_then(|policy| serde_json::to_string(policy).ok())
                .unwrap_or_default(),
        }
    }
}

/// Remove the admission keys from a driver table. The gateway owns the
/// policy: it reads and validates it, and sends it in `Configure`.
pub fn strip_gateway_owned_keys(table: &mut toml::map::Map<String, toml::Value>) {
    for key in ["allow_driver_config", "resource_admission"] {
        table.remove(key);
    }
}

#[cfg(test)]
mod tests;
