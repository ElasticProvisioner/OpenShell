// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Launch and `Configure` protocol for gateway-launched compute drivers.
//!
//! A launched driver serves [`Launch`] next to its own services. The driver's
//! services are wrapped in [`Deferred`], which answers `UNAVAILABLE` until
//! `Configure` has succeeded once.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use tonic::{Request, Response, Status};

#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    unused_qualifications
)]
pub mod proto {
    tonic::include_proto!("openshell.driver.launch.v1");
}

pub use proto::driver_launch_server::{DriverLaunch, DriverLaunchServer};
pub use proto::{
    CollectorTls, ConfigureRequest, ConfigureResponse, GatewayContext, GetLaunchInfoRequest,
    GetLaunchInfoResponse, ImageOverrides, NetworkTrust, OtlpExport, ShutdownRequest,
    ShutdownResponse, Telemetry,
};

/// The launch protocol version this crate implements.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest `ConfigureRequest` a driver accepts.
pub const MAX_CONFIGURE_REQUEST_BYTES: usize = 1024 * 1024;

/// Encode `request` for the wire.
#[must_use]
pub fn encode_configure_request(request: &ConfigureRequest) -> Vec<u8> {
    prost::Message::encode_to_vec(request)
}

/// Decode a `ConfigureRequest`, rejecting bytes over the size limit.
pub fn decode_configure_request(bytes: &[u8]) -> Result<ConfigureRequest, Status> {
    if bytes.len() > MAX_CONFIGURE_REQUEST_BYTES {
        return Err(Status::out_of_range("ConfigureRequest is too large"));
    }
    prost::Message::decode(bytes)
        .map_err(|error| Status::invalid_argument(format!("invalid ConfigureRequest: {error}")))
}

/// Set when the gateway releases the driver with `Shutdown`.
///
/// A launched driver stops serving once this is set, once its gateway is gone,
/// or once a stop signal from elsewhere has waited out a grace period.
/// Cloning shares the same signal.
#[derive(Clone, Debug)]
pub struct Shutdown(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for Shutdown {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(false).0))
    }
}

impl Shutdown {
    /// A signal that has not been set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the signal.
    pub fn request(&self) {
        self.0.send_replace(true);
    }

    /// Whether the signal has been set.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        *self.0.borrow()
    }

    /// Wait until the signal is set.
    pub async fn requested(&self) {
        let mut receiver = self.0.subscribe();
        // The sender lives as long as this handle, so the wait cannot fail.
        let _ = receiver.wait_for(|requested| *requested).await;
    }
}

/// State a driver builds from a successful `Configure`.
///
/// Cloning shares the same cell.
pub struct Configured<S>(Arc<OnceLock<S>>);

impl<S> Clone for Configured<S> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S> Default for Configured<S> {
    fn default() -> Self {
        Self(Arc::new(OnceLock::new()))
    }
}

impl<S> Configured<S> {
    /// The state, once `Configure` has succeeded.
    #[must_use]
    pub fn get(&self) -> Option<&S> {
        self.0.get()
    }
}

/// What a driver reports from `GetLaunchInfo`.
#[derive(Debug, Clone)]
pub struct LaunchInfo {
    pub driver_name: String,
    pub driver_version: String,
    pub features: Vec<String>,
}

type ConfigureFuture<S> = Pin<Box<dyn Future<Output = Result<S, Status>> + Send>>;
type ConfigureFn<S> = Box<dyn Fn(ConfigureRequest) -> ConfigureFuture<S> + Send + Sync>;

/// Server for [`DriverLaunch`].
///
/// `configure` runs after the request passes the protocol checks. It either
/// returns the driver's configured state or a status. Any failure leaves the
/// driver running and unconfigured, so a later call can retry; the gateway
/// does not retry. Calls are serialized, and once one has succeeded later
/// calls fail with `FAILED_PRECONDITION`.
pub struct Launch<S> {
    info: LaunchInfo,
    configured: Configured<S>,
    configure: ConfigureFn<S>,
    gate: tokio::sync::Mutex<()>,
    shutdown: Shutdown,
}

impl<S: Send + Sync + 'static> Launch<S> {
    pub fn new<F, Fut>(info: LaunchInfo, configure: F) -> Self
    where
        F: Fn(ConfigureRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<S, Status>> + Send + 'static,
    {
        Self {
            info,
            configured: Configured::default(),
            configure: Box::new(move |request| Box::pin(configure(request))),
            gate: tokio::sync::Mutex::new(()),
            shutdown: Shutdown::new(),
        }
    }

    /// A handle to the state `Configure` produces, for [`Deferred`].
    #[must_use]
    pub fn configured(&self) -> Configured<S> {
        self.configured.clone()
    }

    /// The signal `Shutdown` sets, for the driver's server to stop on.
    #[must_use]
    pub fn shutdown(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// The gRPC service, limited to [`MAX_CONFIGURE_REQUEST_BYTES`].
    #[must_use]
    pub fn into_server(self) -> DriverLaunchServer<Self> {
        DriverLaunchServer::new(self).max_decoding_message_size(MAX_CONFIGURE_REQUEST_BYTES)
    }
}

#[tonic::async_trait]
impl<S: Send + Sync + 'static> DriverLaunch for Launch<S> {
    async fn get_launch_info(
        &self,
        _request: Request<GetLaunchInfoRequest>,
    ) -> Result<Response<GetLaunchInfoResponse>, Status> {
        Ok(Response::new(GetLaunchInfoResponse {
            driver_name: self.info.driver_name.clone(),
            driver_version: self.info.driver_version.clone(),
            protocol_versions: vec![PROTOCOL_VERSION],
            features: self.info.features.clone(),
        }))
    }

    async fn configure(
        &self,
        request: Request<ConfigureRequest>,
    ) -> Result<Response<ConfigureResponse>, Status> {
        let request = request.into_inner();
        let _serialized = self.gate.lock().await;
        if self.configured.get().is_some() {
            return Err(Status::failed_precondition("driver is already configured"));
        }
        if request.protocol_version != PROTOCOL_VERSION {
            return Err(Status::failed_precondition(format!(
                "unsupported protocol version {}; this driver supports {PROTOCOL_VERSION}",
                request.protocol_version
            )));
        }
        if let Some(feature) = request
            .required_features
            .iter()
            .find(|feature| !self.info.features.contains(feature))
        {
            return Err(Status::failed_precondition(format!(
                "unsupported required feature {feature:?}"
            )));
        }
        let state = (self.configure)(request).await?;
        // The gate makes this the only writer.
        let _ = self.configured.0.set(state);
        Ok(Response::new(ConfigureResponse {}))
    }

    async fn shutdown(
        &self,
        _request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        self.shutdown.request();
        Ok(Response::new(ShutdownResponse {}))
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Wraps a driver service so it answers `UNAVAILABLE` until `Configure`
/// succeeds, then delegates to the service built from the configured state.
pub struct Deferred<Svc, S> {
    configured: Configured<S>,
    build: Arc<dyn Fn(&S) -> Svc + Send + Sync>,
    service: Arc<OnceLock<Svc>>,
}

impl<Svc, S> Deferred<Svc, S> {
    pub fn new(
        configured: Configured<S>,
        build: impl Fn(&S) -> Svc + Send + Sync + 'static,
    ) -> Self {
        Self {
            configured,
            build: Arc::new(build),
            service: Arc::new(OnceLock::new()),
        }
    }
}

impl<Svc, S> Clone for Deferred<Svc, S> {
    fn clone(&self) -> Self {
        Self {
            configured: self.configured.clone(),
            build: Arc::clone(&self.build),
            service: Arc::clone(&self.service),
        }
    }
}

impl<Svc: tonic::server::NamedService, S> tonic::server::NamedService for Deferred<Svc, S> {
    const NAME: &'static str = Svc::NAME;
}

impl<Svc, S> tower::Service<http::Request<tonic::body::Body>> for Deferred<Svc, S>
where
    Svc: tower::Service<
            http::Request<tonic::body::Body>,
            Response = http::Response<tonic::body::Body>,
            Error = Infallible,
        > + Clone
        + Send
        + Sync
        + 'static,
    Svc::Future: Send + 'static,
    S: Send + Sync + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let Some(state) = self.configured.get() else {
            return Box::pin(std::future::ready(Ok(Status::unavailable(
                "driver is not configured yet",
            )
            .into_http())));
        };
        let mut service = self.service.get_or_init(|| (self.build)(state)).clone();
        Box::pin(async move { tower::Service::call(&mut service, request).await })
    }
}

#[cfg(test)]
mod tests;
