//! Compile-time endpoint metadata for the discovered route table (R3).
//!
//! # Why this module exists
//!
//! The playground listing has to report the method and path the router *actually mounts*.
//! A second, hand-maintained table (or a build-time re-read of the route files) drifts:
//! the router serves `PUT` while the listing still says `POST`. [`service_routes!`]
//! removes that gap by generating BOTH halves of a route file from one entry list, so a
//! route author has exactly one place to write a path, a verb, a handler and (optionally)
//! a port:
//!
//! * the route-file contract's
//!   `pub fn register(Router<AppState>, Option<u16>) -> Router<AppState>`, and
//! * `pub const ENDPOINTS: &[EndpointMeta]` of the same module, same entries, same order.
//!
//! No parsing is involved on either side: `build_support/` never reads route-file *contents*
//! (it only walks file names), so a renamed path segment cannot be "found" in one view and
//! missed in the other; it is a single token used twice.
//!
//! # The shape of the declaration (path first, then verbs, then port)
//!
//! The entry list is deliberately path-first with a **method-keyed group** of handlers, and
//! an optional trailing `, port:`. That is the structure of the TypeScript standard this
//! package mirrors, item for item - not its JSON, only its structure:
//!
//! * `packages/underload/service/src/server/start.ts:183-210` - a route module default
//!   exports `{ route, handler, port? }`: the route path first, the handler second, the port
//!   optional and last.
//! * `packages/underload/service/src/server/handler.ts:31-46` - the verbs are grouped
//!   *inside* one handler (`asServiceHandler({ GET: getVariables, PUT: putVariables })`), so
//!   one path owns one handler object keyed by verb.
//! * Real example: `packages/underload/service/runtime/endpoint/preferences/endpoints/service-route.ts:8-14`.
//!
//! The Rust spelling of the same three slots is
//! `"$path" => { VERB: handler, "summary"; }, port: N;` where the brace group IS the `handler:`
//! slot (its keys are the verbs) and `, port:` is the `port?` slot. Rust uses a macro instead of
//! a JSON object because that is the notation this crate can type-check at compile time, and the
//! summary string is the extra slot the Rust listing needs (the TS playground reads its summary
//! from the endpoint's OpenAPI tags).
//!
//! # What `port` means
//!
//! A port is NOT part of the URL: it selects **which listener** the group mounts on. The same
//! API path may therefore be declared twice, once per port, and neither declaration overrides
//! the other - the two listeners are separate routers.
//!
//! * No `, port:` - the group belongs to the default listener, spelled `Option::None` in the
//!   generated `register_routes(router, port)` / `register(router, port)` chain (the bin's
//!   `0.0.0.0:8000` default is `src/bin/server.rs`, i.e. the caller's business, not this macro's).
//! * `, port: 8010` - the group mounts ONLY into the router built for 8010 (`Some(8010)`).
//!
//! A group whose port does not match the requested one contributes no `.route()` call at all,
//! so the path does not exist on that listener (a plain 404 from the fallback, never a router
//! 405 for a path that is not served there).
//!
//! # The `ANY` verb
//!
//! A verb name in the entry list is normally `axum::routing::MethodFilter::<VERB>`, and the
//! listing prints `stringify!` of it. `ANY` is the one entry whose router half is NOT a
//! `MethodFilter`: axum 0.8.9 defines no `MethodFilter::Any` constant (only `CONNECT`, `DELETE`,
//! `GET`, `HEAD`, `OPTIONS`, `PATCH`, `POST`, `PUT`, `TRACE` - `src/routing/method_filter.rs:29-45`),
//! and its verb-agnostic mount is the free function `axum::routing::any`
//! (`src/routing/method_routing.rs:508`). [`service_routes!`] therefore resolves `ANY` through
//! the crate-root helper macro `__service_route_method_router!`, which produces
//! `axum::routing::any(handler)`; every other verb
//! keeps the `MethodFilter` path unchanged. The metadata half needs no special case: the token
//! `ANY` stringifies to `"ANY"`, which is the listing's method spelling for a mount that answers
//! every verb and lets the endpoint itself decide the verb test.
//!
//! That is exactly what the `/v1/harness` bridge (`packages/typecrate/harness/endpoint/harness/
//! harness-route.rs`, T6) needs: the harness dispatcher prints the 405 sentence naming the
//! allowed verbs of the route IT matched, and the stream mount answers a `GET` that carries no
//! upgrade headers with its own 400 - a router-level 405 would replace both.
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
//! * **No filesystem paths.** [`EndpointMeta`] is three `&'static str` fields plus an optional
//!   `u16` port, so a serialised listing can never leak the absolute scan root or a `#[path]`
//!   value the way the generated file's *comments* do. This is the same rule `src/server/mod.rs`
//!   states for `AppState::database_root`: paths stay process-local.
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
//!   (lines 292-300) and are therefore not in the listing at all. `app()` is the DEFAULT
//!   listener, i.e. it asks `crate::register_routes(router, None)` for the unported groups.
//! * `build_support/emit.rs` emits crate-scope `pub fn register_routes(router, port)` and
//!   `pub fn route_metadata() -> Vec<crate::server::EndpointMeta>` (the listing is the full
//!   table, every port in one response) and is the only generator of those call surfaces; it
//!   concatenates each discovered module's `ENDPOINTS` in registration order. The contract
//!   strings (generated function name, const name, macro path) live in `build_support/mod.rs`
//!   and are pinned by `tests/discovery.rs`.
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
/// `{"method":...,"path":...,"summary":...}` for a group that declares no port, and
/// `{"method":...,"path":...,"summary":...,"port":N}` for one that does. `port` is last AND
/// `skip_serializing_if = "Option::is_none"`, which is what keeps every pre-port listing row
/// byte-identical instead of growing a `"port": null` on endpoints that never asked for one.
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
///   without inventing a key type. `port` participates, so `GET /x` on the default listener
///   and `GET /x` on 8010 are two distinct sortable rows rather than one duplicate.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EndpointMeta {
    /// Uppercase HTTP method, e.g. `"GET"`.
    ///
    /// Not a free string: [`service_routes!`] fills it with `stringify!` of the very
    /// identifier it uses for `axum::routing::MethodFilter::<METHOD>`, so an unknown verb
    /// fails to compile on the route file instead of being listed wrongly.
    ///
    /// `"ANY"` is the one value that is not a wire verb: it is the token of a verb-agnostic
    /// mount (`axum::routing::any`), i.e. "every verb reaches this handler, and the endpoint
    /// itself decides which ones it answers". The `/v1/harness` rows are the only ones written
    /// that way, because the harness dispatcher owns its own 405 sentences (T6).
    pub method: &'static str,

    /// Route pattern exactly as handed to `Router::route`, `{param}` segments included
    /// (axum 0.8 / matchit 0.8 spelling, e.g. `"/v1/services/echo/{value}"`).
    pub path: &'static str,

    /// One-line human description for the playground.
    ///
    /// The only field with no compile-time cross-check (it is prose, not wiring), and it is
    /// mandatory so no listed endpoint can be self-explanatory to its author only. When the
    /// same path is declared on two ports, the two summaries are what tell the listener apart
    /// in the listing, so they must not be copy-paste of each other.
    pub summary: &'static str,

    /// The listener this endpoint is served on: `None` = the default listener, `Some(N)` =
    /// the group that declared `, port: N`.
    ///
    /// Not part of the URL and not a second mount of the same thing: two rows with the same
    /// method+path and different ports are two independent endpoints, one per listener
    /// (the TS standard's `port?` slot, `start.ts:183-210`). `None` is skipped during
    /// serialisation, so the pre-port wire shape is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// Declares the endpoints of one discovered route file, and derives that module's
/// `ENDPOINTS` metadata from the same entries.
///
/// Grammar - one entry per **path group**, each group ending in `;`, and inside it one line
/// per mounted verb of that path:
///
/// ```text
/// crate::service_routes! {
///     "/v1/services/health" => {
///         GET: health, "Reports the service name and version";
///     };
///
///     "/v1/services/health" => {
///         GET: health, "Reports the service name and version on the isolated 8010 listener";
///     }, port: 8010;
/// }
/// ```
///
/// as a matcher sketch:
/// `$( $path:literal => { $( $method:ident : $handler:path , $summary:literal ; )* } $( , port: $port:literal )? ; )*`
/// - route path first, then the dedicated method-keyed group of handlers, then the optional
///   port, mirroring `{ route, handler: { VERB: fn }, port? }` of the TypeScript standard
///   (see the module docs for the exact files).
///
/// The expansion is three macros deep, and the middle one exists only because of a
/// macro_rules limitation worth knowing before editing it: a meta-variable bound inside an
/// OPTIONAL group cannot be repeated alongside a variable of a sibling repetition
/// ("meta-variable `method` repeats 2 times, but `port` repeats 0 times"), and the listing
/// needs the group's port once per verb. So:
///
/// 1. [`service_routes!`] hands the raw token stream to [`__service_route_normalize!`], which
///    munches one group per step into the canonical form `[ "/path" ] [ 8010 ] { VERB: .. ; } ;`
///    (an unported group gets the empty token tree `[]` in the port slot), accumulating groups
///    in the order they were written;
/// 2. [`__service_route_emit!`] matches that canonical form, in which the port slot is a
///    MANDATORY single token tree and therefore binds at the same depth as the path - which is
///    precisely what lets the per-verb rows read it;
/// 3. [`__service_route_declared_port!`] turns the slot into the `Option<u16>` both halves use.
///
/// The grammar an author writes is unaffected, and so is the error surface: a group that is
/// neither `"$path" => { ... } ;` nor `"$path" => { ... } , port: N ;` matches no muncher arm
/// and is a compile error pointing at the route file.
///
/// * `"route pattern"` (first) is the literal handed to `Router::route` *and* the literal in
///   `EndpointMeta::path` - one token, two uses.
/// * The `{ ... }` **group is mandatory**, even for a single verb. It is the `handler:` slot,
///   and requiring it keeps the grammar unambiguous next to the trailing `, port:` and gives
///   one-verb and many-verb paths the same shape.
/// * `METHOD` is an identifier and the only place the verb is written. Registration uses
///   `axum::routing::MethodFilter::METHOD`
///   (`axum-0.8.9/src/routing/method_filter.rs:29-45` defines `CONNECT`, `DELETE`, `GET`,
///   `HEAD`, `OPTIONS`, `PATCH`, `POST`, `PUT`, `TRACE`); the metadata uses
///   `stringify!(METHOD)`. Any other spelling is a compile error on the route file, so the
///   listing cannot be wrong about a verb it did compile with.
/// * `ANY` is the one METHOD with no `MethodFilter` constant: it mounts
///   `axum::routing::any(handler)` (see the module docs and
///   [`__service_route_method_router!`]) and is listed as the literal `"ANY"`, because
///   `stringify!(ANY)` is `"ANY"`. A verb-agnostic mount answers no verb test at the router, so
///   the endpoint owns the 405/400 sentences - which is what the `/v1/harness` bridge needs.
/// * `handler` is a `path` fragment (`handler`, `self::handler`, `module::handler`): any axum
///   handler fn in scope. Its own signature is checked by axum's `Handler` impl, so a bad
///   extractor is a normal rustc error pointing at this file.
/// * `"summary"` is the playground's one-line description (see [`EndpointMeta::summary`]) and
///   is written per verb, because each verb is a separate listing row.
/// * `, port: N` (optional, LAST) declares which listener the whole group mounts on - see the
///   "What `port` means" section of the module docs. Omitted means the default listener. The
///   slot is resolved once per group (both halves ask [`__service_route_declared_port!`] for
///   the `Option<u16>` it describes), so a group's verbs can never disagree about it.
///
/// Repeating a path with *different* methods is supported and is the intended way to write
/// it: axum merges the two `MethodRouter`s for an already-seen path
/// (`axum-0.8.9/src/routing/path_router.rs:90-104`, the documented
/// `.route("/", get(_)).route("/", post(_))` case). The same method *and* path *and* port twice
/// is reported by axum at table-build time - `Router::route` is `#[track_caller]` and calls
/// `panic_on_err!` (`axum-0.8.9/src/routing/mod.rs:176-182`, overlapping-method message in
/// `axum-0.8.9/src/routing/method_routing.rs:838-844`, reached through `Router::merge`'s
/// `panic_on_err!` at `mod.rs:268`) - so a duplicate is a loud startup failure, never a silent
/// override. The same method+path on **different** ports is NOT a duplicate: only the matching
/// group reaches `Router::route` for a given listener, which is what lets one API path be
/// published on two ports at once.
///
/// Order is load-bearing and is exactly the written order: groups in the order they appear in
/// the file, and inside a group the verbs in the order they are written. Both halves come from
/// the same repetition, so the `.route()` chain and the `ENDPOINTS` rows stay in step.
///
/// An empty invocation is legal and is what a placeholder route file compiles to: `register`
/// becomes the identity and `ENDPOINTS` the empty slice. A group with no verbs is legal for
/// the same reason (it contributes no mount and no row).
///
/// Scope: usable from **inside this crate** only (discovered route files are modules of it -
/// `build_support/mod.rs` "Route file contract"), because the emitted signature names
/// `$crate::server::AppState`. `#[macro_export]` is what makes it reachable as
/// `crate::service_routes!` with no `use` statement in a route file, and `$crate` keeps that
/// spelling correct however the crate is renamed or depended upon.
#[macro_export]
macro_rules! service_routes {
    // The surface grammar is documented on the macro itself; the token stream is forwarded
    // untouched to the muncher, whose first argument is the (initially empty) accumulator.
    ( $( $input:tt )* ) => {
        $crate::__service_route_normalize! { [] ; $( $input )* }
    };
}

/// One munching step of [`service_routes!`]: read the next group off the input, append it to
/// the accumulator in canonical form, and recurse on what is left.
///
/// Canonical form of one group (what [`__service_route_emit!`] matches):
///
/// ```text
/// [ "/v1/services/pets" ] [ 8010 ] { GET: list, "Lists every stored pet"; };
/// ```
///
/// * the path in its own bracket group, so the emitter reads it as one `literal`;
/// * the port slot in its own bracket group: `[ 8010 ]` when the group wrote `, port: 8010`,
///   and the single empty token tree `[]` when it wrote nothing. Making the slot ALWAYS present
///   is the whole point of this macro: a variable bound inside an optional repeat cannot be
///   used inside the emitter's per-verb repeat (rustc's repetition-alignment check), while a
///   mandatory single `tt` bound once per group can, exactly like the path;
/// * the `{ ... }` handler group verbatim, which the emitter destructures into verbs.
///
/// Arms are tried in order and the ported arm comes first because it is the longer match: a
/// group without a port fails its literal `,` and falls through to the third arm. The recursion
/// is one level per written group (three at most in this repository), so the macro recursion
/// limit is nowhere near involved.
#[macro_export]
#[doc(hidden)]
macro_rules! __service_route_normalize {
    // Terminal: the input is exhausted, so emit over everything accumulated so far.
    ( [ $( $acc:tt )* ] ; ) => {
        $crate::__service_route_emit! { $( $acc )* }
    };

    // A group with `, port: N`, appended as `[ path ] [ N ] handlers ;`.
    ( [ $( $acc:tt )* ] ;
      $path:literal => $handlers:tt , port: $port:literal ;
      $( $rest:tt )* ) => {
        $crate::__service_route_normalize! {
            [ $( $acc )* [ $path ] [ $port ] $handlers ; ] ;
            $( $rest )*
        }
    };

    // A group on the default listener, appended with the empty token tree `[]` as its port
    // slot - the same shape the emitter matches either way.
    ( [ $( $acc:tt )* ] ;
      $path:literal => $handlers:tt ;
      $( $rest:tt )* ) => {
        $crate::__service_route_normalize! {
            [ $( $acc )* [ $path ] [] $handlers ; ] ;
            $( $rest )*
        }
    };
}

/// The emitter of [`service_routes!`]: it only ever sees canonical groups (see
/// [`__service_route_normalize!`]) and produces the two items of the route-file contract from
/// them, in the order they were written.
///
/// `$port_slot` is the mandatory bracketed slot (`[ N ]` or `[]`), bound once per group at the
/// same depth as `$path` - which is what makes it readable inside the per-verb repetition.
#[macro_export]
#[doc(hidden)]
macro_rules! __service_route_emit {
    (
        $( [ $path:literal ] $port_slot:tt
           { $( $method:ident : $handler:path , $summary:literal ; )* } ; )*
    ) => {
        /// Route-file contract: the crate-scope `register_routes` emitted by `build.rs`
        /// chains this function (`build_support/emit.rs`), passing the listener it is
        /// building so each group can decide whether it belongs there.
        pub fn register(
            router: axum::Router<$crate::server::AppState>,
            port: ::core::option::Option<u16>,
        ) -> axum::Router<$crate::server::AppState> {
            // `port` is read by every group below. Naming it once up front is what keeps the
            // EMPTY invocation (a placeholder route file) warning-free under `-D warnings`,
            // where there is no group left that would read it.
            let _ = port;

            // One `.merge()` per written group, in the order written, chained from the
            // PARAMETER. Why `merge` of a one-group router rather than an `if` around a
            // `.route()` call: the chain has to stay an EXPRESSION (binding the parameter and
            // re-binding it per group is `clippy::let_and_return`, and this expansion is
            // linted by `npm run lint:strict`), and a group that does not belong to `port`
            // must contribute NO path node - an empty `MethodRouter` at that path would
            // answer 405 instead of the fallback 404. `Router::merge` of an empty router is a
            // verified no-op (`axum-0.8.9/src/routing/path_router.rs:162-201` walks the other
            // router's route list, so an empty one inserts nothing), and merging a non-empty
            // one goes through the very same `PathRouter::route` as a direct `.route()` call -
            // identical merge/panic semantics, identical insert order.
            router
            $(
                .merge(
                    // The group's declared port (`None` when it wrote no `, port:`) is the
                    // listener it belongs to: compare against the requested one.
                    if port == $crate::__service_route_declared_port!($port_slot) {
                        axum::Router::new()
                        $(
                            .route(
                                $path,
                                $crate::__service_route_method_router!($method, $handler),
                            )
                        )*
                    } else {
                        axum::Router::new()
                    }
                )
            )*
        }

        /// Every endpoint this route file registers, on every listener, in the order written
        /// above (groups first, then the verbs inside each group).
        ///
        /// The crate-scope `route_metadata()` (`build_support/emit.rs`) concatenates these
        /// slices in registration order; the values are the same tokens the `register`
        /// function above mounts. The listing is NOT filtered by port: one row per mounted
        /// `(method, path, port)`, so the playground can show the same path published twice.
        pub const ENDPOINTS: &[$crate::server::EndpointMeta] = &[$(
            $(
                $crate::server::EndpointMeta {
                    // The verb the router was built with, rendered as its HTTP spelling.
                    method: ::core::stringify!($method),
                    path: $path,
                    summary: $summary,
                    // `None` for the default listener, `Some(N)` for a `, port: N` group.
                    port: $crate::__service_route_declared_port!($port_slot),
                },
            )*
        )*];
    };
}

/// The declared port of one [`service_routes!`] group as an `Option<u16>`: `Some(N)` for the
/// `[ N ]` slot that a `, port: N` group produces, `None` for the `[]` slot of an unported one.
///
/// Why this exists at all: both halves of the expansion need the same decision - the router
/// half compares it against the listener being built, the metadata half stores it in
/// [`EndpointMeta::port`] - and an absent port has to become the VALUE `None`, never a missing
/// struct field or a `Some()`. Resolving it in one place is also what keeps the listing rows of
/// unported routes byte-identical to what they were before the port slot existed.
///
/// A slot of any other shape matches no arm, so the grammar stays compile-enforced.
///
/// `#[doc(hidden)]` + `#[macro_export]` for the same reason as
/// [`__service_route_method_router!`]: it is crate-internal plumbing that has to live at the
/// CRATE ROOT to be reachable as `$crate::...` from a discovered route module.
#[macro_export]
#[doc(hidden)]
macro_rules! __service_route_declared_port {
    // The group declared `, port: N`, so it belongs to listener N only.
    ( [ $port:literal ] ) => {
        ::core::option::Option::Some($port)
    };
    // The group declared no port, so it belongs to the default listener.
    ( [] ) => {
        ::core::option::Option::None
    };
}

/// Verb-to-`MethodRouter` resolution for one [`service_routes!`] entry, so the entry list stays
/// the only place a verb is written while `ANY` and the nine `MethodFilter` verbs still mount
/// through their own axum constructor.
///
/// Why a helper macro instead of one uniform expression: axum 0.8.9 has no
/// `MethodFilter::Any` constant to plug into `axum::routing::on(..)` - its verb-agnostic mount is
/// the free function `axum::routing::any` (`src/routing/method_routing.rs:508`). One expression of
/// the `on(..)` shape cannot cover both entries, because `ANY` names a function rather than a
/// constant.
///
/// Matching the literal `ANY` first and falling through to the `$method:ident` arm keeps every
/// existing verb's emitted code byte-identical, supports a mixed list (`ANY` next to `GET` in one
/// file), and leaves the failure mode unchanged: an unknown verb is still a compile error on the
/// route file, at the `MethodFilter::NOPE` path, never a wrong listing row.
///
/// `#[doc(hidden)]` + `#[macro_export]` because it is crate-internal plumbing that has to live at
/// the CRATE ROOT to be reachable as `$crate::...` from a discovered route module (a route file
/// has no `use` statement at all - see `build_support/mod.rs` "Route file contract").
#[macro_export]
#[doc(hidden)]
macro_rules! __service_route_method_router {
    // The verb-agnostic mount: no `MethodFilter`, no router-level 405, no `allow` header.
    (ANY, $handler:path) => {
        axum::routing::any($handler)
    };
    // Every real verb, exactly as before this helper existed.
    ($method:ident, $handler:path) => {
        axum::routing::on(axum::routing::MethodFilter::$method, $handler)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A route file written through the macro, exercising the shapes the contract promises:
    /// two methods grouped under one path (POST + PUT on `/probe/v1`), a `{param}` pattern,
    /// and a reused handler.
    mod probe {
        crate::service_routes! {
            "/probe/v1/{id}" => {
                GET: read_probe, "Reads one probe row";
            };

            "/probe/v1" => {
                POST: write_probe, "Creates a probe row";
                PUT: write_probe, "Replaces the probe row";
            };
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

    /// A group with the mandatory braces and NO verb lines: legal, and contributes neither a
    /// mount nor a listing row (it is the same identity the empty invocation has).
    mod empty_group_probe {
        crate::service_routes! {
            "/probe/empty-group" => {};
        }
    }

    /// The verb-agnostic form, written the way the `/v1/harness` bridge writes it (T6): three
    /// `ANY` entries, a `{*rest}` pattern, a reused handler, and ONE real verb (`GET`) mixed in -
    /// which is the shape that would break if `ANY` had to reuse `axum::routing::MethodFilter`.
    mod any_probe {
        crate::service_routes! {
            "/probe/any" => {
                ANY: read_any, "Answers every verb on this path";
            };

            "/probe/any/{*rest}" => {
                ANY: read_any, "Answers every verb under this prefix";
            };

            "/probe/mixed" => {
                GET: read_any, "A declared verb beside the ANY mounts";
            };
        }

        async fn read_any() {}
    }

    /// One API path declared TWICE, once per listener - the rule the port slot exists for.
    /// Neither group overrides the other: the default listener gets `read_default`, port 8010
    /// gets `read_lan`, and building either router is panic-free because only the matching
    /// group ever reaches `Router::route`.
    mod two_ports_probe {
        crate::service_routes! {
            "/probe/port" => {
                GET: read_default, "Served on the default listener";
            };

            "/probe/port" => {
                GET: read_lan, "Served on the isolated 8010 listener";
            }, port: 8010;
        }

        async fn read_default() {}
        async fn read_lan() {}
    }

    /// The same path split across TWO groups on the SAME listener - the shape a route file gets
    /// when a second verb is written as its own group instead of another line in the first one.
    /// axum merges the method routers of a path it has already seen, so the two groups mount one
    /// path with two verbs and nothing panics (this is the case the old flat entry list covered
    /// with adjacent lines).
    mod shared_path_probe {
        crate::service_routes! {
            "/probe/shared" => {
                GET: read_shared, "Reads the shared probe row";
            };

            "/probe/shared" => {
                POST: write_shared, "Writes the shared probe row";
            };
        }

        async fn read_shared() {}
        async fn write_shared() {}
    }

    /// A route file whose ONLY group is ported, which is the shape that has to produce a
    /// completely empty router when someone asks for the default listener.
    mod port_only_probe {
        crate::service_routes! {
            "/probe/port-only" => {
                GET: read_port, "Served on the isolated 8011 listener";
            }, port: 8011;
        }

        async fn read_port() {}
    }

    /// The duplicate the contract still has to make loud: the same verb, the same path AND the
    /// same port twice. Both groups match the requested listener, so axum sees the same
    /// `GET /probe/dup` mount twice and reports it while the table is built.
    mod duplicate_probe {
        crate::service_routes! {
            "/probe/dup" => {
                GET: read_dup, "First declaration of this mount";
            };

            "/probe/dup" => {
                GET: read_dup, "Second declaration of the same mount on the same port";
            };
        }

        async fn read_dup() {}
    }
    /// `ANY` is metadata `\"ANY\"` plus `axum::routing::any`, and it coexists with a `MethodFilter`
    /// verb in the same entry list: both halves come from the same tokens, in the order written.
    ///
    /// That the router really answers an undeclared verb is proven over a socket, not here -
    /// `tests/harness_gateway.rs` (a `DELETE` and an unregistered method on `/v1/harness/...`) and
    /// `tests/server_http.rs` (the dispatch plan sends the literal `ANY`). This test pins the two
    /// things only the macro can pin: the listing token and the fact that the emitted `register`
    /// compiles and mounts every entry.
    #[test]
    fn the_any_verb_lists_the_literal_any_and_mounts_alongside_a_real_verb() {
        assert_eq!(
            any_probe::ENDPOINTS,
            &[
                EndpointMeta {
                    method: "ANY",
                    path: "/probe/any",
                    summary: "Answers every verb on this path",
                    port: None,
                },
                EndpointMeta {
                    method: "ANY",
                    path: "/probe/any/{*rest}",
                    summary: "Answers every verb under this prefix",
                    port: None,
                },
                EndpointMeta {
                    method: "GET",
                    path: "/probe/mixed",
                    summary: "A declared verb beside the ANY mounts",
                    port: None,
                },
            ]
        );
        // Building the table is where axum would reject a bad `MethodRouter` (a duplicate
        // method+path pair panics here), so `has_routes()` on a mixed list is the router half.
        let router = any_probe::register(axum::Router::<crate::server::AppState>::new(), None);
        assert!(router.has_routes());
    }

    /// An empty invocation must produce the identity `register` and an empty `ENDPOINTS`,
    /// i.e. the generator can concatenate a module that contributes nothing.
    #[test]
    fn an_empty_invocation_registers_nothing_and_lists_nothing() {
        // `register` has to leave a route-less router route-less (the generated
        // `register_routes` chains it, so an identity here is the whole contract) - on either
        // listener, because an empty module belongs to neither and both.
        let router = axum::Router::<crate::server::AppState>::new();
        assert!(!empty_probe::register(router, None).has_routes());
        // Typed binding: the annotation is what makes `&[]` an empty slice of
        // `EndpointMeta` rather than an inference puzzle.
        let none: &[EndpointMeta] = &[];
        assert_eq!(empty_probe::ENDPOINTS, none);
    }

    /// The mandatory `{}` group may legitimately hold no verb: it mounts nothing and lists
    /// nothing, on every listener.
    #[test]
    fn a_group_without_any_verb_mounts_nothing_and_lists_nothing() {
        let router = axum::Router::<crate::server::AppState>::new();
        assert!(!empty_group_probe::register(router, None).has_routes());
        let none: &[EndpointMeta] = &[];
        assert_eq!(empty_group_probe::ENDPOINTS, none);
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
                    port: None,
                },
                EndpointMeta {
                    method: "POST",
                    path: "/probe/v1",
                    summary: "Creates a probe row",
                    port: None,
                },
                EndpointMeta {
                    method: "PUT",
                    path: "/probe/v1",
                    summary: "Replaces the probe row",
                    port: None,
                },
            ]
        );
    }

    /// Building the router is where axum validates the table, so a `register` call that
    /// returns with routes present proves the same-path/different-method entries merged
    /// (a duplicate method+path would panic here instead).
    #[test]
    fn the_macro_mounts_the_same_endpoints_it_lists() {
        let router = probe::register(axum::Router::<crate::server::AppState>::new(), None);
        assert!(router.has_routes());
        // Three entries, two distinct paths: the listing length is the entry count, so a
        // merged route is still listed per method (the playground shows verbs, not paths).
        assert_eq!(probe::ENDPOINTS.len(), 3);
    }

    /// A ported group is listed with its port and mounts ONLY there: the default listener
    /// receives an untouched router, the declared port receives it, and a third port receives
    /// nothing either. Nothing here is a "route with a null port" - the path simply does not
    /// exist on a listener that did not ask for it.
    #[test]
    fn a_ported_group_mounts_only_on_the_port_it_declares() {
        assert_eq!(
            port_only_probe::ENDPOINTS,
            &[EndpointMeta {
                method: "GET",
                path: "/probe/port-only",
                summary: "Served on the isolated 8011 listener",
                port: Some(8011),
            }]
        );
        let router = axum::Router::<crate::server::AppState>::new();
        assert!(!port_only_probe::register(router, None).has_routes());
        let router = axum::Router::<crate::server::AppState>::new();
        assert!(!port_only_probe::register(router, Some(8010)).has_routes());
        let router = axum::Router::<crate::server::AppState>::new();
        assert!(port_only_probe::register(router, Some(8011)).has_routes());
    }

    /// The same API path declared on two ports is legal and produces two listing rows that
    /// differ only by `port`; building each listener is panic-free, and each listener gets its
    /// own handler (no override, no duplicate-route panic).
    #[test]
    fn the_same_path_declared_on_two_ports_is_two_endpoints_and_builds_on_both() {
        assert_eq!(
            two_ports_probe::ENDPOINTS,
            &[
                EndpointMeta {
                    method: "GET",
                    path: "/probe/port",
                    summary: "Served on the default listener",
                    port: None,
                },
                EndpointMeta {
                    method: "GET",
                    path: "/probe/port",
                    summary: "Served on the isolated 8010 listener",
                    port: Some(8010),
                },
            ]
        );
        let default =
            two_ports_probe::register(axum::Router::<crate::server::AppState>::new(), None);
        assert!(default.has_routes());
        let lan =
            two_ports_probe::register(axum::Router::<crate::server::AppState>::new(), Some(8010));
        assert!(lan.has_routes());
    }

    /// Two groups that share one path on one listener mount the SAME path with the union of
    /// their verbs (axum merges the method routers), and the listing still keeps both entries.
    #[test]
    fn two_groups_sharing_a_path_on_one_listener_merge_instead_of_conflicting() {
        assert_eq!(
            shared_path_probe::ENDPOINTS,
            &[
                EndpointMeta {
                    method: "GET",
                    path: "/probe/shared",
                    summary: "Reads the shared probe row",
                    port: None,
                },
                EndpointMeta {
                    method: "POST",
                    path: "/probe/shared",
                    summary: "Writes the shared probe row",
                    port: None,
                },
            ]
        );
        let router =
            shared_path_probe::register(axum::Router::<crate::server::AppState>::new(), None);
        assert!(router.has_routes());
    }

    /// The duplicate guarantee survives the port slot: the same verb, path AND port twice is
    /// still a loud startup failure (axum's overlapping-method message), not a silent override.
    #[test]
    #[should_panic(expected = "Overlapping method route")]
    fn the_same_verb_path_and_port_twice_panics_when_the_table_is_built() {
        // The listing is still two rows (the macro derives metadata from the entries, it does
        // not know they collide) - the collision is a ROUTER fact, and it fires while the
        // second group's `.merge()` is applied, i.e. before a `Router` could be returned.
        assert_eq!(
            duplicate_probe::ENDPOINTS,
            &[
                EndpointMeta {
                    method: "GET",
                    path: "/probe/dup",
                    summary: "First declaration of this mount",
                    port: None,
                },
                EndpointMeta {
                    method: "GET",
                    path: "/probe/dup",
                    summary: "Second declaration of the same mount on the same port",
                    port: None,
                },
            ]
        );
        let router =
            duplicate_probe::register(axum::Router::<crate::server::AppState>::new(), None);
        assert!(router.has_routes());
    }

    /// The serialised playground row of an unported endpoint is exactly the three declared
    /// keys, in declaration order, with no module identifier or filesystem path anywhere in it
    /// (R3: the listing is safe to serve on the LAN) - i.e. byte-identical to the pre-port wire
    /// shape, because `port` is `skip_serializing_if = "Option::is_none"`.
    #[test]
    fn an_unported_endpoint_serialises_as_method_path_summary_only() {
        let meta = EndpointMeta {
            method: "GET",
            path: "/v1/services/echo/{value}",
            summary: "Echoes `{value}`, with `?prefix=` applied when present",
            port: None,
        };
        assert_eq!(
            serde_json::to_string(&[meta]).expect("endpoint metadata is serialisable"),
            r#"[{"method":"GET","path":"/v1/services/echo/{value}","summary":"Echoes `{value}`, with `?prefix=` applied when present"}]"#
        );
    }

    /// A ported endpoint gains exactly one extra key, LAST: the row is otherwise the same
    /// three keys in the same order, so a listing reader keeps parsing without a change while
    /// still learning which listener the endpoint is served on.
    #[test]
    fn a_ported_endpoint_serialises_port_last() {
        let meta = EndpointMeta {
            method: "GET",
            path: "/v1/services/health",
            summary: "Reports the service name and version on the isolated 8010 listener",
            port: Some(8010),
        };
        assert_eq!(
            serde_json::to_string(&[meta]).expect("endpoint metadata is serialisable"),
            r#"[{"method":"GET","path":"/v1/services/health","summary":"Reports the service name and version on the isolated 8010 listener","port":8010}]"#
        );
    }
}
