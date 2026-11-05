//! Implementation half of the `echo` sample: the request DTOs and the two handler fns. The
//! routes that mount them are declared in `echo-route.rs`, the sibling file; nothing in here
//! names a verb, a path or a summary.
//!
//! How this file is compiled at all: it is **not** a route file, because the scanner picks up
//! only `*-route.rs` names (`build_support/matching.rs`) and ignores every other file in the
//! tree silently. The route file pulls it in with `#[path = "echo-core.rs"] mod handlers;`,
//! so `rustc` reads it as a module nested *inside* the route module
//! (`<route module>::handlers`) and it never appears in the generated table on its own. That
//! is also the rule an implementation file must keep: naming this one `echo-route.rs` would
//! make the scanner discover it as a second route module, and the generated table asks every
//! discovered module for `register` AND `ENDPOINTS`.
//!
//! Both endpoints are the smallest pair in `examples/endpoints/` - no state, no storage, no
//! failure path - so what they show is the extractor surface: `Query` for an optional
//! `?prefix=` and `Path` for a `{value}` segment. `echo_json` is the `POST`, `echo_path` the
//! `GET`; the route file names both in one `crate::service_routes!` block, which is what
//! makes those two entries simultaneously the mount table and the playground listing (R3).
//!
//! # Why these items are `pub` while the module itself is private
//!
//! The route file's `crate::service_routes!` block emits `pub fn register(..)`
//! (`src/server/metadata.rs`), and that fn hands the handler fns below to
//! `axum::routing::on`, so every type in a handler signature lands in a public interface: a
//! private one is a hard rustc error (`E0446`, private type in a public interface) plus the
//! `private_interfaces` warning, measured on this crate. `EchoParams`, `EchoRequest` and
//! `EchoBody` are therefore `pub` even though nothing outside the route module can name them
//! (`mod handlers;` itself is private). Only the module doc and that rule are documented
//! here; the items keep their own doc comments below.

use axum::Json;
use axum::extract::Path;
use axum::extract::Query;
use serde::Deserialize;
use serde::Serialize;

/// `?prefix=` is optional: absent or empty both mean the empty string, so
/// `{value: prefix + value}` holds for every request shape. Unknown query keys are
/// tolerated on purpose (a versioned client may add them).
#[derive(Deserialize)]
pub struct EchoParams {
    #[serde(default)]
    prefix: Option<String>,
}

/// `POST /v1/services/echo` body: `{ "value": "..." }`.
#[derive(Deserialize)]
pub struct EchoRequest {
    value: String,
}

/// Response of both echo endpoints: `{ "value": ... }`, the exact shape the native HTTP
/// test pins.
#[derive(Serialize)]
pub struct EchoBody {
    value: String,
}

/// `GET /v1/services/echo/{value}` -> `{ "value": "<prefix><value>" }`.
///
/// `{value}` is axum 0.8 / matchit 0.8 path syntax; the old `:value` spelling would be a
/// literal segment and 404.
pub async fn echo_path(
    Path(value): Path<String>,
    Query(params): Query<EchoParams>,
) -> Json<EchoBody> {
    let prefix = params.prefix.unwrap_or_default();
    Json(EchoBody {
        value: format!("{prefix}{value}"),
    })
}

/// `POST /v1/services/echo` -> the posted `value` back, unchanged (no prefix: a JSON body
/// carries its own state, and the contract says this endpoint is the identity one).
pub async fn echo_json(Json(request): Json<EchoRequest>) -> Json<EchoBody> {
    Json(EchoBody {
        value: request.value,
    })
}
