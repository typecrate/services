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
//! Both HTTP methods of one resource are declared here, in one block: repeating a path with a
//! *different* verb is the intended spelling (axum merges the two method routers). Being
//! compiled at all is the proof the build-time scan reaches axum - a wrong verb or a wrong
//! path literal is a compile error on this file, never a runtime failure. The generated
//! `register_routes` chains this module's `register` fn and the generated `route_metadata`
//! concatenates this module's `ENDPOINTS`, so the two entries below are simultaneously the
//! mount table and the playground listing (R3).

#[path = "echo-core.rs"]
mod handlers;

use handlers::echo_json;
use handlers::echo_path;

// Route-file contract (`build_support/mod.rs`), one entry per mounted method+path pair.
// Entry order is both the `.route()` chain order and the `ENDPOINTS` order, so the listing
// reads top-down exactly like the table it describes. Two entries sharing one path with
// different methods are fine (axum merges the method routers); the same method+path twice
// is an axum panic while the table is built. `{value}` is part of the listed path, so the
// playground shows the pattern, not a sample URL (`src/server/metadata.rs`). Both handler
// names resolve through the `use` lines above: the macro expands in this module, which is
// where those names are in scope, not in the core file.
crate::service_routes! {
    POST "/v1/services/echo" => echo_json, "Echoes the posted `value` unchanged";
    GET "/v1/services/echo/{value}" => echo_path, "Echoes `{value}`, with `?prefix=` applied";
}
