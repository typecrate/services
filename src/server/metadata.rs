//! Compile-time endpoint metadata for the discovered route table (R3).
//!
//! # Why this module exists
//!
//! The playground listing has to report the method and path the router *actually mounts*.
//! A second, hand-maintained table (or a build-time re-read of the route files) drifts:
//! the router serves `PUT` while the listing still says `POST`. [`service_routes!`]
//! removes that gap by generating BOTH halves of a route file from one entry list, so a
//! route author has exactly one place to write a verb, a path and a handler:
//!
//! * the route-file contract's `pub fn register(Router<AppState>) -> Router<AppState>`, and
//! * `pub const ENDPOINTS: &[EndpointMeta]` of the same module, same entries, same order.
//!
//! No parsing is involved on either side: `build_support/` never reads route-file *contents*
//! (it only walks file names), so a renamed path segment cannot be "found" in one view and
//! missed in the other; it is a single token used twice.
//!
//! # What is NOT here
//!
//! * **No runtime registry / no mutable global.** `ENDPOINTS` is a `pub const` slice and the
//!   crate-scope `route_metadata()` (`build_support/emit.rs`, generated) concatenates those
//!   slices at call time from `&'static` data. The listing therefore exists exactly as long
//!   as the routes it describes, and cannot be observed half-updated.
//! * **No built-ins.** `route_metadata()` walks the *discovered* modules only; routes owned
//!   by `src/server` itself (the playground page) are not part of the discovered set and
//!   are described where they are defined.
//! * **No filesystem paths.** [`EndpointMeta`] is three `&'static str` fields, so a
//!   serialised listing can never leak the absolute scan root or a `#[path]` value the way
//!   the generated file's *comments* do. This is the same rule `src/server/mod.rs` states
//!   for `AppState::database_root`: paths stay process-local.
//! * **No wasm.** This module lives under `pub mod server`, which `src/lib.rs` compiles only
//!   for non-wasm targets, so `serde` still does not reach the wasm artifact (R4).
//!
//! # Wiring (who names what)
//!
//! * `src/server/mod.rs:42-43` declares `mod metadata;` and `pub use metadata::EndpointMeta;`,
//!   which is why both this file's macro and the generated table spell the type
//!   `crate::server::EndpointMeta`.
//! * `src/server/mod.rs` owns the ONLY reader: `ROUTE_METADATA_PATH` (`"/v1/services/routes"`,
//!   line 167) is served by `route_metadata_listing` (lines 247-262), which calls
//!   `crate::route_metadata()` and serialises the `Vec<EndpointMeta>` as JSON. The built-ins
//!   (`/`, `/playground.css`, `/playground.js`, that route) are registered in `app()`
//!   (lines 292-300) and are therefore not in the listing at all.
//! * `build_support/emit.rs` emits crate-scope `pub fn route_metadata() -> Vec<crate::server::EndpointMeta>`
//!   (lines 161-190) and is the only generator of that call surface; it concatenates each
//!   discovered module's `ENDPOINTS` in registration order. The contract strings (generated
//!   function name, const name, macro path) live in `build_support/mod.rs` and are pinned by
//!   `tests/discovery.rs`.
//! * Route files call `crate::service_routes!` instead of writing `register` by hand -
//!   see `examples/endpoints/health/health-route.rs` and
//!   `examples/endpoints/echo/echo-route.rs`. Only a `*-route.rs` name is discovered; the
//!   `<stem>-core.rs` implementation file beside it is compiled because the route file pulls
//!   it in with its own `#[path]`, never because the scanner matched its name. Every
//!   discovered route file MUST use it
//!   (or expose `pub const ENDPOINTS` by hand), because the generated listing references
//!   `<module>::ENDPOINTS` for every discovered module: a missing const is a normal rustc
//!   error pointing at the generated function.

use serde::Serialize;

/// One discovered HTTP endpoint: the row the playground lists, and the JSON it lists it as.
///
/// Field order is serialisation order, so the published shape is
/// `{"method":...,"path":...,"summary":...}`.
///
/// Derives, each for a concrete consumer:
/// * `Serialize` - the playground response (this is the only type in the crate whose
///   *fields* are HTTP-visible metadata rather than a request/response body).
/// * `Clone`/`Copy` - `route_metadata()` builds an owned `Vec` out of `&'static` slices;
///   `Copy` also means the compiler picks the cheap bitwise path instead of a `.clone()`
///   call per row.
/// * `Debug`/`PartialEq`/`Eq` - tests assert whole values (`assert_eq!` on a full
///   `Vec<EndpointMeta>`), which is how this crate pins an exact contract instead of a
///   fuzzy shape.
/// * `Ord`/`Hash` - a listing may be sorted or de-duplicated by the presentation layer
///   without inventing a key type.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EndpointMeta {
    /// Uppercase HTTP method, e.g. `"GET"`.
    ///
    /// Not a free string: [`service_routes!`] fills it with `stringify!` of the very
    /// identifier it uses for `axum::routing::MethodFilter::<METHOD>`, so an unknown verb
    /// fails to compile on the route file instead of being listed wrongly.
    pub method: &'static str,

    /// Route pattern exactly as handed to `Router::route`, `{param}` segments included
    /// (axum 0.8 / matchit 0.8 spelling, e.g. `"/v1/services/echo/{value}"`).
    pub path: &'static str,

    /// One-line human description for the playground.
    ///
    /// The only field with no compile-time cross-check (it is prose, not wiring), and it is
    /// mandatory so no listed endpoint can be self-explanatory to its author only.
    pub summary: &'static str,
}

/// Declares the endpoints of one discovered route file, and derives that module's
/// `ENDPOINTS` metadata from the same entries.
///
/// Grammar - one entry per mounted `(method, path)` pair, each entry ending in `;`:
///
/// ```text
/// crate::service_routes! {
///     GET "/v1/services/health" => health, "Reports the service name and version";
///
///     POST "/v1/services/echo" => echo_json, "Echoes the posted `value` unchanged";
///     GET  "/v1/services/echo/{value}" => echo_path, "Echoes `{value}`, `?prefix=` applied";
/// }
/// ```
///
/// * `METHOD` is an identifier and the only place the verb is written. Registration uses
///   `axum::routing::MethodFilter::METHOD`
///   (`axum-0.8.9/src/routing/method_filter.rs:29-45` defines `CONNECT`, `DELETE`, `GET`,
///   `HEAD`, `OPTIONS`, `PATCH`, `POST`, `PUT`, `TRACE`); the metadata uses
///   `stringify!(METHOD)`. Any other spelling is a compile error on the route file, so the
///   listing cannot be wrong about a verb it did compile with.
/// * `"route pattern"` is the literal handed to `Router::route` *and* the literal in
///   `EndpointMeta::path` - one token, two uses.
/// * `handler` is a `path` fragment (`handler`, `self::handler`, `module::handler`): any axum
///   handler fn in scope. Its own signature is checked by axum's `Handler` impl, so a bad
///   extractor is a normal rustc error pointing at this file.
/// * `"summary"` is the playground's one-line description (see [`EndpointMeta::summary`]).
///
/// Repeating a path with *different* methods is supported and is the intended way to write
/// it: axum merges the two `MethodRouter`s for an already-seen path
/// (`axum-0.8.9/src/routing/path_router.rs:90-104`, the documented
/// `.route("/", get(_)).route("/", post(_))` case). The same method *and* path twice is
/// reported by axum at table-build time - `Router::route` is `#[track_caller]` and calls
/// `panic_on_err!` (`axum-0.8.9/src/routing/mod.rs:176-182`, overlapping-method message in
/// `axum-0.8.9/src/routing/method_routing.rs:838-844`) - so a duplicate is a loud startup
/// failure, never a silent override.
///
/// An empty invocation is legal and is what a placeholder route file compiles to: `register`
/// becomes the identity and `ENDPOINTS` the empty slice.
///
/// Scope: usable from **inside this crate** only (discovered route files are modules of it -
/// `build_support/mod.rs` "Route file contract"), because the emitted signature names
/// `$crate::server::AppState`. `#[macro_export]` is what makes it reachable as
/// `crate::service_routes!` with no `use` statement in a route file, and `$crate` keeps that
/// spelling correct however the crate is renamed or depended upon.
#[macro_export]
macro_rules! service_routes {
    (
        $( $method:ident $path:literal => $handler:path, $summary:literal ; )*
    ) => {
        /// Route-file contract: the crate-scope `register_routes` emitted by `build.rs`
        /// chains this function (`build_support/emit.rs`).
        pub fn register(
            router: axum::Router<$crate::server::AppState>,
        ) -> axum::Router<$crate::server::AppState> {
            // One `.route()` per entry, in the order written, chained from the PARAMETER:
            // an empty invocation leaves the parameter itself as the body (no
            // `clippy::let_and_return` in a `#[deny]`-warnings gate), and the order is the
            // order `route_metadata` below lists, because both come from the same repetition.
            router
            $(
                .route(
                    $path,
                    axum::routing::on(axum::routing::MethodFilter::$method, $handler),
                )
            )*
        }

        /// Every endpoint this route file registers, in the order written above.
        ///
        /// The crate-scope `route_metadata()` (`build_support/emit.rs`) concatenates these
        /// slices in registration order; the values are the same tokens the `register`
        /// function above mounts.
        pub const ENDPOINTS: &[$crate::server::EndpointMeta] = &[$(
            $crate::server::EndpointMeta {
                // The verb the router was built with, rendered as its HTTP spelling.
                method: ::core::stringify!($method),
                path: $path,
                summary: $summary,
            },
        )*];
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A route file written through the macro, exercising the shapes the contract promises:
    /// two methods sharing one path (POST + PUT on `/probe/v1`), a `{param}` pattern, and a
    /// reused handler.
    mod probe {
        crate::service_routes! {
            GET "/probe/v1/{id}" => read_probe, "Reads one probe row";
            POST "/probe/v1" => write_probe, "Creates a probe row";
            PUT "/probe/v1" => write_probe, "Replaces the probe row";
        }

        async fn read_probe() {}
        async fn write_probe() {}
    }

    /// The zero-entry form, which is what a route file that registers nothing compiles to
    /// (`build_support/emit.rs` still declares the module, and `route_metadata` still has to
    /// be able to concatenate it).
    mod empty_probe {
        crate::service_routes! {}
    }

    /// An empty invocation must produce the identity `register` and an empty `ENDPOINTS`,
    /// i.e. the generator can concatenate a module that contributes nothing.
    #[test]
    fn an_empty_invocation_registers_nothing_and_lists_nothing() {
        // `register` has to leave a route-less router route-less (the generated
        // `register_routes` chains it, so an identity here is the whole contract).
        let router = axum::Router::<crate::server::AppState>::new();
        assert!(!empty_probe::register(router).has_routes());
        // Typed binding: the annotation is what makes `&[]` an empty slice of
        // `EndpointMeta` rather than an inference puzzle.
        let none: &[EndpointMeta] = &[];
        assert_eq!(empty_probe::ENDPOINTS, none);
    }

    /// Metadata is exactly what the entries say - same order, same literals - and the verb
    /// is the `stringify!` of the identifier used for `MethodFilter`, not a re-typed string.
    #[test]
    fn the_macro_lists_every_entry_in_the_order_it_mounts_them() {
        assert_eq!(
            probe::ENDPOINTS,
            &[
                EndpointMeta {
                    method: "GET",
                    path: "/probe/v1/{id}",
                    summary: "Reads one probe row",
                },
                EndpointMeta {
                    method: "POST",
                    path: "/probe/v1",
                    summary: "Creates a probe row",
                },
                EndpointMeta {
                    method: "PUT",
                    path: "/probe/v1",
                    summary: "Replaces the probe row",
                },
            ]
        );
    }

    /// Building the router is where axum validates the table, so a `register` call that
    /// returns with routes present proves the same-path/different-method entries merged
    /// (a duplicate method+path would panic here instead).
    #[test]
    fn the_macro_mounts_the_same_endpoints_it_lists() {
        let router = probe::register(axum::Router::<crate::server::AppState>::new());
        assert!(router.has_routes());
        // Three entries, two distinct paths: the listing length is the entry count, so a
        // merged route is still listed per method (the playground shows verbs, not paths).
        assert_eq!(probe::ENDPOINTS.len(), 3);
    }

    /// The serialised playground row is exactly the three declared keys, in declaration
    /// order, with no module identifier or filesystem path anywhere in it (R3: the listing
    /// is safe to serve on the LAN).
    #[test]
    fn endpoint_metadata_serialises_as_method_path_summary_only() {
        let meta = EndpointMeta {
            method: "GET",
            path: "/v1/services/echo/{value}",
            summary: "Echoes `{value}`, with `?prefix=` applied when present",
        };
        assert_eq!(
            serde_json::to_string(&[meta]).expect("endpoint metadata is serialisable"),
            r#"[{"method":"GET","path":"/v1/services/echo/{value}","summary":"Echoes `{value}`, with `?prefix=` applied when present"}]"#
        );
    }
}
