//! Drop-in health-check router.
//!
//! Apps that want to satisfy the Shiku HTTP-health contract (per
//! `[health.http]` in shiku.toml) can `.merge(shiku_runtime::health::router())`
//! into their axum app. The default endpoint is `/api/health` returning
//! `200 OK` with the body `"ok"`.
//!
//! For services that need a richer readiness check (DB connectivity, etc.),
//! use [`router_with`] and supply a closure that returns `Result<(),
//! anyhow::Error>` — failure becomes a 503.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};

/// Default health endpoint path. Matches the deploy doc default.
pub const DEFAULT_PATH: &str = "/api/health";

/// Liveness-only health router: returns `200 OK` whenever the process is
/// running enough to handle a request. Suitable for most services.
pub fn router() -> Router {
    Router::new().route(DEFAULT_PATH, get(|| async { (StatusCode::OK, "ok") }))
}

/// Readiness-aware health router. The supplied closure is invoked on every
/// probe; a `Ok(())` returns 200, an `Err` returns 503 with the error message.
///
/// The closure is `Fn() -> impl Future` so it can capture connection pools,
/// shared state, etc. Wrap state in `Arc` if you need it.
pub fn router_with<F, Fut>(check: F) -> Router
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let check: CheckFn = Arc::new(move || Box::pin(check()));
    Router::new()
        .route(DEFAULT_PATH, get(probe))
        .with_state(check)
}

/// Type alias for the readiness-check closure stored in router state.
/// Public because it shows up in router signatures.
pub type CheckFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

async fn probe(State(check): State<CheckFn>) -> Result<axum::response::Response, Infallible> {
    let response = match check().await {
        Ok(()) => (StatusCode::OK, "ok".to_string()).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("not ready: {e:#}")).into_response(),
    };
    Ok(response)
}
