//! Implementation half of the `health` sample: the response DTO and the handler fn. The
//! route that mounts them is declared in `health-route.rs`, the sibling file; nothing in
//! here names a verb, a path or a summary.
//!
//! This file is never discovered by name - the scanner takes only `*-route.rs` files
//! (`build_support/matching.rs`) - so it compiles exactly because the route file pulls it in
//! with `#[path = "health-core.rs"] mod handlers;` and it becomes
//! `<route module>::handlers`, invisible in the generated table. Naming this file
//! `health-route.rs` instead would be the mistake to avoid: the scanner would then discover
//! it as a route module of its own, and the generated table asks every discovered module for
//! `register` AND `ENDPOINTS`.
//!
//! The endpoint keeps no state, so the whole implementation is one struct with two `&'static
//! str` fields filled from Cargo metadata - the pair `src/lib.rs` asserts the crate name and
//! version agree on.
//!
//! # Why `HealthResponse` is `pub` while the module itself is private
//!
//! The route file's `crate::service_routes!` block emits `pub fn register(..)`
//! (`src/server/metadata.rs`), and that fn hands `health` to `axum::routing::on`, so every
//! type in a handler signature is part of a public interface: a private one is a hard rustc
//! error (`E0446`, private type in a public interface) plus the `private_interfaces`
//! warning. `mod handlers;` is itself private, so nothing outside the route module can name
//! the type either way.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

/// `GET /v1/services/health` response: exactly the two fields the native HTTP test pins
/// (adding a field is a contract change).
#[derive(Serialize)]
pub struct HealthResponse {
    service: &'static str,
    version: &'static str,
}

/// Health probe.
///
/// `State<crate::server::AppState>` is taken to prove the shared state reaches a generated
/// route (it is what makes `AppState: Clone + Send + Sync + 'static` a compile-time
/// requirement, i.e. the requirement `Router<AppState>` imposes anyway). The body stays
/// `{"service","version"}` because the contract pins it exactly, so the binding is unused
/// on purpose.
pub async fn health(State(_state): State<crate::server::AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        // Cargo metadata is the single source of truth: `service` is the crate name (npm
        // name with `@` dropped and `/` -> `-`), `version` is Cargo.toml == package.json,
        // the pair `src/lib.rs` asserts on.
        service: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    })
}
