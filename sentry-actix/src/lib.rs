//! This crate adds a middleware for [`actix-web`](https://actix.rs/) that captures errors and
//! report them to `Sentry`.
//!
//! To use this middleware just configure Sentry and then add it to your actix web app as a
//! middleware.  Because actix is generally working with non sendable objects and highly concurrent
//! this middleware creates a new hub per request.  As a result many of the sentry integrations
//! such as breadcrumbs do not work unless you bind the actix hub.
//!
//! # Example
//!
//! ```no_run
//! use std::io;
//!
//! use actix_web::{get, App, Error, HttpRequest, HttpServer};
//!
//! #[get("/")]
//! async fn failing(_req: HttpRequest) -> Result<String, Error> {
//!     Err(io::Error::new(io::ErrorKind::Other, "An error happens here").into())
//! }
//!
//! #[actix_web::main]
//! async fn main() -> io::Result<()> {
//!     let _guard = sentry::init(());
//!     std::env::set_var("RUST_BACKTRACE", "1");
//!
//!     HttpServer::new(|| {
//!         App::new()
//!             .wrap(sentry_actix::Sentry::new())
//!             .service(failing)
//!     })
//!     .bind("127.0.0.1:3001")?
//!     .run()
//!     .await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Using Release Health
//!
//! The actix middleware will automatically start a new session for each request
//! when `auto_session_tracking` is enabled and the client is configured to
//! use `SessionMode::Request`.
//!
//! ```
//! let _sentry = sentry::init(sentry::ClientOptions {
//!     session_mode: sentry::SessionMode::Request,
//!     auto_session_tracking: true,
//!     ..Default::default()
//! });
//! ```
//!
//! # Reusing the Hub
//!
//! This integration will automatically create a new per-request Hub from the main Hub, and update the
//! current Hub instance. For example, the following will capture a message in the current request's Hub:
//!
//! ```
//! sentry::capture_message("Something is not well", sentry::Level::Warning);
//! ```

#![doc(html_favicon_url = "https://sentry-brand.storage.googleapis.com/favicon.ico")]
#![doc(html_logo_url = "https://sentry-brand.storage.googleapis.com/sentry-glyph-black.png")]
#![warn(missing_docs)]
#![allow(deprecated)]
#![allow(clippy::type_complexity)]

use std::borrow::Cow;
use std::pin::Pin;
use std::sync::Arc;

use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use futures_util::future::{ok, Future, Ready};
use futures_util::FutureExt;

use sentry_core::protocol::{ClientSdkPackage, Event, Request};
use sentry_core::{Hub, SentryFutureExt};

/// A helper construct that can be used to reconfigure and build the middleware.
pub struct SentryBuilder {
    middleware: Sentry,
}

impl SentryBuilder {
    /// Finishes the building and returns a middleware
    pub fn finish(self) -> Sentry {
        self.middleware
    }

    /// Reconfigures the middleware so that it uses a specific hub instead of the default one.
    pub fn with_hub(mut self, hub: Arc<Hub>) -> Self {
        self.middleware.hub = Some(hub);
        self
    }

    /// Reconfigures the middleware so that it uses a specific hub instead of the default one.
    pub fn with_default_hub(mut self) -> Self {
        self.middleware.hub = None;
        self
    }

    /// If configured the sentry id is attached to a X-Sentry-Event header.
    pub fn emit_header(mut self, val: bool) -> Self {
        self.middleware.emit_header = val;
        self
    }

    /// Enables or disables error reporting.
    ///
    /// The default is to report all errors.
    pub fn capture_server_errors(mut self, val: bool) -> Self {
        self.middleware.capture_server_errors = val;
        self
    }
}

/// Reports certain failures to Sentry.
#[derive(Clone)]
pub struct Sentry {
    hub: Option<Arc<Hub>>,
    emit_header: bool,
    capture_server_errors: bool,
}

impl Sentry {
    /// Creates a new sentry middleware.
    pub fn new() -> Self {
        Sentry {
            hub: None,
            emit_header: false,
            capture_server_errors: true,
        }
    }

    /// Creates a new middleware builder.
    pub fn builder() -> SentryBuilder {
        Sentry::new().into_builder()
    }

    /// Converts the middleware into a builder.
    pub fn into_builder(self) -> SentryBuilder {
        SentryBuilder { middleware: self }
    }
}

impl Default for Sentry {
    fn default() -> Self {
        Sentry::new()
    }
}

impl<S, B> Transform<S, ServiceRequest> for Sentry
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = actix_web::Error>,
    S::Future: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = SentryMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(SentryMiddleware {
            service,
            inner: self.clone(),
        })
    }
}

/// The middleware for individual services.
pub struct SentryMiddleware<S> {
    service: S,
    inner: Sentry,
}

impl<S, B> Service<ServiceRequest> for SentryMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = actix_web::Error>,
    S::Future: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let inner = self.inner.clone();
        let hub = Arc::new(Hub::new_from_top(
            inner.hub.clone().unwrap_or_else(Hub::main),
        ));
        let client = hub.client();
        let track_sessions = client.as_ref().map_or(false, |client| {
            let options = client.options();
            options.auto_session_tracking
                && options.session_mode == sentry_core::SessionMode::Request
        });
        if track_sessions {
            hub.start_session();
        }
        let with_pii = client
            .as_ref()
            .map_or(false, |client| client.options().send_default_pii);

        let (tx, sentry_req) = sentry_request_from_http(&req, with_pii);
        hub.configure_scope(|scope| {
            scope.set_transaction(tx.as_deref());
            scope.add_event_processor(Box::new(move |event| {
                Some(process_event(event, &sentry_req))
            }))
        });

        let fut = self.service.call(req).bind_hub(hub.clone());

        async move {
            // Service errors
            let mut res: Self::Response = match fut.await {
                Ok(res) => res,
                Err(e) => {
                    if inner.capture_server_errors {
                        hub.capture_error(&e);
                    }
                    return Err(e);
                }
            };

            // Response errors
            if inner.capture_server_errors && res.response().status().is_server_error() {
                if let Some(e) = res.response().error() {
                    let event_id = hub.capture_error(e);

                    if inner.emit_header {
                        res.response_mut().headers_mut().insert(
                            "x-sentry-event".parse().unwrap(),
                            event_id.to_simple_ref().to_string().parse().unwrap(),
                        );
                    }
                }
            }

            Ok(res)
        }
        .boxed_local()
    }
}

/// Build a Sentry request struct from the HTTP request
fn sentry_request_from_http(request: &ServiceRequest, with_pii: bool) -> (Option<String>, Request) {
    let transaction = if let Some(name) = request.match_name() {
        Some(String::from(name))
    } else {
        request.match_pattern()
    };

    let mut sentry_req = Request {
        url: format!(
            "{}://{}{}",
            request.connection_info().scheme(),
            request.connection_info().host(),
            request.uri()
        )
        .parse()
        .ok(),
        method: Some(request.method().to_string()),
        headers: request
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
            .collect(),
        ..Default::default()
    };

    // If PII is enabled, include the remote address
    if with_pii {
        if let Some(remote) = request.connection_info().remote_addr() {
            sentry_req.env.insert("REMOTE_ADDR".into(), remote.into());
        }
    };

    (transaction, sentry_req)
}

/// Add request data to a Sentry event
fn process_event(mut event: Event<'static>, request: &Request) -> Event<'static> {
    // Request
    if event.request.is_none() {
        event.request = Some(request.clone());
    }

    // SDK
    if let Some(sdk) = event.sdk.take() {
        let mut sdk = sdk.into_owned();
        sdk.packages.push(ClientSdkPackage {
            name: "sentry-actix".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        });
        event.sdk = Some(Cow::Owned(sdk));
    }
    event
}

#[cfg(test)]
mod tests {
    use std::io;

    use actix_web::test::{call_service, init_service, TestRequest};
    use actix_web::{get, web, App, HttpRequest, HttpResponse};
    use futures::executor::block_on;

    use sentry::Level;

    use super::*;

    fn _assert_hub_no_events() {
        if Hub::current().last_event_id().is_some() {
            panic!("Current hub should not have had any events.");
        }
    }

    fn _assert_hub_has_events() {
        Hub::current()
            .last_event_id()
            .expect("Current hub should have had events.");
    }

    /// Test explicit events sent to the current Hub inside an Actix service.
    #[actix_rt::test]
    async fn test_explicit_events() {
        let events = sentry::test::with_captured_events(|| {
            block_on(async {
                let service = || {
                    // Current Hub should have no events
                    _assert_hub_no_events();

                    sentry::capture_message("Message", Level::Warning);

                    // Current Hub should have the event
                    _assert_hub_has_events();

                    HttpResponse::Ok()
                };

                let app = init_service(
                    App::new()
                        .wrap(Sentry::builder().with_hub(Hub::current()).finish())
                        .service(web::resource("/test").to(service)),
                )
                .await;

                // Call the service twice (sequentially) to ensure the middleware isn't sticky
                for _ in 0..2 {
                    let req = TestRequest::get().uri("/test").to_request();
                    let res = call_service(&app, req).await;
                    assert!(res.status().is_success());
                }
            })
        });

        assert_eq!(events.len(), 2);
        for event in events {
            let request = event.request.expect("Request should be set.");
            assert_eq!(event.transaction, Some("/test".into()));
            assert_eq!(event.message, Some("Message".into()));
            assert_eq!(event.level, Level::Warning);
            assert_eq!(request.method, Some("GET".into()));
        }
    }

    /// Ensures errors returned in the Actix service trigger an event.
    #[actix_rt::test]
    async fn test_response_errors() {
        let events = sentry::test::with_captured_events(|| {
            block_on(async {
                #[get("/test")]
                async fn failing(_req: HttpRequest) -> Result<String, actix_web::Error> {
                    // Current hub should have no events
                    _assert_hub_no_events();

                    Err(io::Error::new(io::ErrorKind::Other, "Test Error").into())
                }

                let app = init_service(
                    App::new()
                        .wrap(Sentry::builder().with_hub(Hub::current()).finish())
                        .service(failing),
                )
                .await;

                // Call the service twice (sequentially) to ensure the middleware isn't sticky
                for _ in 0..2 {
                    let req = TestRequest::get().uri("/test").to_request();
                    let res = call_service(&app, req).await;
                    assert!(res.status().is_server_error());
                }
            })
        });

        assert_eq!(events.len(), 2);
        for event in events {
            let request = event.request.expect("Request should be set.");
            assert_eq!(event.transaction, Some("failing".into())); // Transaction name is the name of the function
            assert_eq!(event.message, None);
            assert_eq!(event.exception.values[0].ty, String::from("Custom"));
            assert_eq!(event.exception.values[0].value, Some("Test Error".into()));
            assert_eq!(event.level, Level::Error);
            assert_eq!(request.method, Some("GET".into()));
        }
    }

    /// Ensures client errors (4xx) are not captured.
    #[actix_rt::test]
    async fn test_client_errors_discarded() {
        let events = sentry::test::with_captured_events(|| {
            block_on(async {
                let service = || HttpResponse::NotFound();

                let app = init_service(
                    App::new()
                        .wrap(Sentry::builder().with_hub(Hub::current()).finish())
                        .service(web::resource("/test").to(service)),
                )
                .await;

                let req = TestRequest::get().uri("/test").to_request();
                let res = call_service(&app, req).await;
                assert!(res.status().is_client_error());
            })
        });

        assert!(events.is_empty());
    }

    /// Ensures transaction name can be overridden in handler scope.
    #[actix_rt::test]
    async fn test_override_transaction_name() {
        let events = sentry::test::with_captured_events(|| {
            block_on(async {
                #[get("/test")]
                async fn original_transaction(
                    _req: HttpRequest,
                ) -> Result<String, actix_web::Error> {
                    // Override transaction name
                    sentry::configure_scope(|scope| scope.set_transaction(Some("new_transaction")));
                    Err(io::Error::new(io::ErrorKind::Other, "Test Error").into())
                }

                let app = init_service(
                    App::new()
                        .wrap(Sentry::builder().with_hub(Hub::current()).finish())
                        .service(original_transaction),
                )
                .await;

                let req = TestRequest::get().uri("/test").to_request();
                let res = call_service(&app, req).await;
                assert!(res.status().is_server_error());
            })
        });

        assert_eq!(events.len(), 1);
        let event = events[0].clone();
        let request = event.request.expect("Request should be set.");
        assert_eq!(event.transaction, Some("new_transaction".into())); // Transaction name is overridden by handler
        assert_eq!(event.message, None);
        assert_eq!(event.exception.values[0].ty, String::from("Custom"));
        assert_eq!(event.exception.values[0].value, Some("Test Error".into()));
        assert_eq!(event.level, Level::Error);
        assert_eq!(request.method, Some("GET".into()));
    }

    #[actix_rt::test]
    async fn test_track_session() {
        let envelopes = sentry::test::with_captured_envelopes_options(
            || {
                block_on(async {
                    #[get("/")]
                    async fn hello() -> impl actix_web::Responder {
                        String::from("Hello there!")
                    }

                    let middleware = Sentry::builder().with_hub(Hub::current()).finish();

                    let app = init_service(App::new().wrap(middleware).service(hello)).await;

                    for _ in 0..5 {
                        let req = TestRequest::get().uri("/").to_request();
                        call_service(&app, req).await;
                    }
                })
            },
            sentry::ClientOptions {
                release: Some("some-release".into()),
                session_mode: sentry::SessionMode::Request,
                auto_session_tracking: true,
                ..Default::default()
            },
        );
        assert_eq!(envelopes.len(), 1);

        let mut items = envelopes[0].items();
        if let Some(sentry::protocol::EnvelopeItem::SessionAggregates(aggregate)) = items.next() {
            let aggregates = &aggregate.aggregates;

            assert_eq!(aggregates[0].distinct_id, None);
            assert_eq!(aggregates[0].exited, 5);
        } else {
            panic!("expected session");
        }
        assert_eq!(items.next(), None);
    }
}
