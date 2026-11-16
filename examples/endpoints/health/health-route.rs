//! Route half of the `health` sample: the `GET /v1/services/health` mount and the two rows
//! `GET /v1/services/routes` answers for it (the default listener and the isolated 8010
//! listener), and nothing else. The response DTO and the
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
//! The endpoints themselves are declared once, in the single `crate::service_routes!`
//! invocation at the bottom of this file: that macro emits both the `register(router, port)`
//! fn the generated `register_routes` chains and this module's `ENDPOINTS` slice that the
//! generated `route_metadata` lists (R3). Nothing in `build_support/` reads this file's
//! contents, so the playground listing has no second copy of the method, the path or the port
//! to fall out of step.

#[path = "health-core.rs"]
mod handlers;

use handlers::health;

// Route-file contract (`build_support/mod.rs`): the path is written first, the verb goes in
// the method-keyed group beside it, and the optional `, port:` trailer says which listener
// the group serves. `GET` drives `axum::routing::MethodFilter::GET` for the mount AND
// `stringify!(GET)` for `EndpointMeta::method`, so a wrong verb is a compile error on this
// file rather than a wrong row in the listing (`src/server/metadata.rs`). `health` is the
// core file's fn, in scope from the `use` line above.
//
// The two groups below are the port rule made visible: the SAME API path is declared once
// without a port (the default listener, i.e. what `app()` serves on the bin's port) and once
// with `port: 8010` (a listener of its own). Neither overrides the other - a router built for
// the default listener mounts only the first group, a router built for 8010 only the second -
// which is why the two summaries differ: the playground lists both rows, and the summary is
// the only thing that tells a reader which listener a row belongs to.
crate::service_routes! {
    "/v1/services/health" => {
        GET: health, "Reports the service name and version";
    };

    "/v1/services/health" => {
        GET: health, "Reports the service name and version on the isolated 8010 listener";
    }, port: 8010;
}
