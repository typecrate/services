//! Route half of the `health` sample: the `GET /v1/services/health` mount and the one row
//! `GET /v1/services/routes` answers for it, and nothing else. The response DTO and the
//! handler fn live in `health-core.rs`, pulled in below.
//!
//! This file is not an `examples/` target - cargo never auto-discovers it (only
//! `examples/*.rs` and `examples/*/main.rs` are targets). It is pulled into the crate by the
//! generated `service_routes.rs`, which emits something like
//! `#[path = "<absolute path>"] mod packages_typecrate_services_examples_endpoints_health_health_route;`
//! because the scanner found it inside an `endpoints/` tree by its `-route.rs` suffix (R3).
//! The module identifier is the scan-root-relative path with every non-`[A-Za-z0-9_]`
//! character replaced by `_`, so the hyphen in the file name becomes an underscore while the
//! file on disk keeps it; it follows the path relative to the **scan root**, so it is long
//! under the default repository root and short when `TYPECRATE_SERVICES_ROUTE_ROOT` points at
//! this package. Nothing should hardcode the identifier: only the generated
//! `register_routes` and `route_metadata` refer to it (`<identifier>::register`,
//! `<identifier>::ENDPOINTS`). This file is the reference for that rule; the other three
//! samples point here instead of repeating it.
//!
//! Being compiled at all is the proof that the build-time scan reaches axum: a broken route
//! is a compile error here, never a runtime failure. The `health-core.rs` beside it is not
//! part of the scan (`*-core.rs` is not `*-route.rs`), so one resource contributes exactly one
//! module and one set of listing rows no matter how many implementation files sit next to it.
//!
//! The endpoint itself is declared once, in the `crate::service_routes!` invocation at the
//! bottom of this file: that macro emits both the `register` fn the generated
//! `register_routes` chains and this module's `ENDPOINTS` slice that the generated
//! `route_metadata` lists (R3). Nothing in `build_support/` reads this file's contents, so
//! the playground listing has no second copy of the method or the path to fall out of step.

#[path = "health-core.rs"]
mod handlers;

use handlers::health;

// Route-file contract (`build_support/mod.rs`): the verb and the path are written exactly
// once, below. `GET` drives `axum::routing::MethodFilter::GET` for the mount AND
// `stringify!(GET)` for `EndpointMeta::method`, so a wrong verb is a compile error on this
// file rather than a wrong row in the listing (`src/server/metadata.rs`). `health` is the
// core file's fn, in scope from the `use` line above.
crate::service_routes! {
    GET "/v1/services/health" => health, "Reports the service name and version";
}
