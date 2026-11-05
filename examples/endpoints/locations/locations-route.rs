//! Route half of the `locations` sample: the six routes this resource mounts, which are also
//! the six rows `GET /v1/services/routes` answers for it, and nothing else. The DTOs, the
//! store helpers and the handler bodies live in `locations-core.rs`, pulled in below.
//!
//! Two things are pinned by this file name and location (see `build_support/matching.rs`):
//! the `-route.rs` suffix inside an `endpoints/` tree is what makes the scanner take this
//! file, and one whole REST resource lives in one route file. Cargo does not auto-discover
//! anything under `examples/` (only `examples/*.rs` and `examples/*/main.rs` are targets), so
//! this file is compiled because the build script's scanner found it there and the generated
//! `service_routes.rs` pulls it in as `#[path = "<absolute path>"] mod ...;` (R3). The module
//! identifier is derived from the scan-root relative path - see
//! `examples/endpoints/health/health-route.rs` - so nothing here may hardcode it. The core
//! file beside this one gets no identifier of its own: its name is not `*-route.rs`, so the
//! scanner ignores it and the resource is still discovered exactly once.
//!
//! All six endpoints are declared in the single `crate::service_routes!` invocation at the
//! bottom of this file: the generated `register_routes` chains this module's `register` fn
//! and the generated `route_metadata` concatenates this module's `ENDPOINTS`, so those
//! entries are simultaneously the mount table and the playground listing (R3). Being compiled
//! at all is the proof the scan reaches axum: a wrong verb, a wrong path or a wrong extractor
//! signature is a compile error - on this file for the first two, on the core file for the
//! last one - never a runtime failure.

#[path = "locations-core.rs"]
mod handlers;

use handlers::create;
use handlers::find;
use handlers::list;
use handlers::remove;
use handlers::replace;
use handlers::update;

// Route-file contract (`build_support/mod.rs`), one entry per mounted method+path pair.
// Entry order is both the `.route()` chain order and the `ENDPOINTS` order, so the listing
// reads top-down exactly like the table it describes. The collection path appears twice
// (GET + POST) and the `{id}` path four times; axum merges the method routers of a path it
// has already seen, so the six entries below mount two path patterns - and the same
// method+path twice would panic while the table is built, not silently override
// (`src/server/metadata.rs`). None of these paths is one of the four reserved built-ins
// (`/`, `/playground.css`, `/playground.js`, `/v1/services/routes`), so nothing here collides
// with `src/server/mod.rs` when `app()` runs. Every handler name resolves through the `use`
// lines above, because the macro expands in this module.
crate::service_routes! {
    GET "/v1/services/locations" => list, "Lists every stored location, `?city=` narrows by exact city";
    POST "/v1/services/locations" => create, "Creates a location and answers 201 with it";
    GET "/v1/services/locations/{id}" => find, "Returns one location by id, or 404 when the id is unknown";
    PUT "/v1/services/locations/{id}" => replace, "Replaces one location by id, or 404 when the id is unknown";
    PATCH "/v1/services/locations/{id}" => update, "Updates only the sent fields of one location, or 404 when the id is unknown";
    DELETE "/v1/services/locations/{id}" => remove, "Removes one location by id and answers 204, or 404 when the id is unknown";
}
