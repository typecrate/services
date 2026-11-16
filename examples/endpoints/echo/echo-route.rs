//! Route half of the `echo` sample: what this file mounts and how it is listed, and nothing
//! else. The DTOs and the handler bodies live in `echo-core.rs`, pulled in below.
//!
//! This file is a route file **because of its name**: the scanner takes a `-route.rs` suffix
//! inside an `endpoint`/`endpoints` tree (`build_support/matching.rs`), and `build.rs` emits
//! `#[path = "<absolute path>"] mod <identifier>;` for it, which is the only reason the crate
//! contains it - cargo never auto-discovers anything under `examples/` (only `examples/*.rs`
//! and `examples/*/main.rs` are targets). The module identifier is derived from the scan-root
//! relative path, so see `examples/endpoints/health/health-route.rs` and nothing here
//! hardcodes it; the core file beside this one has no identifier at all, because the scanner
//! never sees its name.
//!
//! Both HTTP methods of one resource are declared here, in one invocation - each under its own
//! path, with its verb inside that path's handler group: the path-first, verb-grouped shape is
//! the structure of the TypeScript route contract (`{ route, handler: { VERB: fn }, port? }`),
//! restated in macro notation. Being
//! compiled at all is the proof the build-time scan reaches axum - a wrong verb or a wrong
//! path literal is a compile error on this file, never a runtime failure. The generated
//! `register_routes` chains this module's `register(router, port)` fn and the generated
//! `route_metadata` concatenates this module's `ENDPOINTS`, so the two entries below are
//! simultaneously the mount table and the playground listing (R3).

#[path = "echo-core.rs"]
mod handlers;

use handlers::echo_json;
use handlers::echo_path;

// Route-file contract (`build_support/mod.rs`): each group writes its path first and its
// verbs inside the `{}` handler group, one line per mounted method. Group order, and verb
// order inside a group, are both the `.route()` chain order AND the `ENDPOINTS` order, so the
// listing reads top-down exactly like the table it describes. `POST` is written first on
// purpose: it is the written order that is load-bearing, not an alphabetical one. Two groups
// sharing one path with different verbs are fine (axum merges the method routers); the same
// verb+path twice on one port is an axum panic while the table is built. `{value}` is part of
// the listed path, so the playground shows the pattern, not a sample URL
// (`src/server/metadata.rs`). Both handler names resolve through the `use` lines above: the
// macro expands in this module, which is where those names are in scope, not in the core file.
crate::service_routes! {
    "/v1/services/echo" => {
        POST: echo_json, "Echoes the posted `value` unchanged";
    };

    "/v1/services/echo/{value}" => {
        GET: echo_path, "Echoes `{value}`, with `?prefix=` applied";
    };
}
