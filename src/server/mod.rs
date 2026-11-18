//! The native HTTP runtime: shared state + the axum `Router` factory.
//!
//! What lives here and why:
//!
//! * [`AppState`] is the single value every generated handler receives through
//!   axum's `State` extractor. It is cheap to clone (axum requires `Clone` on the
//!   state type) and it is the ONLY place the process learns where writable data
//!   goes - handlers get the root, they never re-derive it from `cwd`.
//! * [`app_for`] hands that state to the build-time generated route table
//!   (`crate::register_routes`, see `src/lib.rs`) FOR ONE LISTENER - the `port` it is
//!   given is the port the route files may declare with their optional `, port:`
//!   trailer (`src/server/metadata.rs`) - so the router a test builds and the router
//!   the executable serves are literally the same function. [`app`] is that factory
//!   for the default listener (`port == None`) and keeps its pre-port signature.
//! * [`declared_ports`] answers "which extra listeners exist at all": the distinct
//!   ports the discovered route table declares. It lives here, next to the router
//!   factory, because the route table is the only source of that answer - the
//!   executable binds exactly this set (`src/bin/server.rs`) rather than configuring
//!   one, so a port cannot be served by a listener that does not know about it or the
//!   other way round.
//! * The browser surface also lives here: the embedded playground document plus the
//!   `GET /v1/services/routes` listing ([`EndpointMeta`]). Both are additive and
//!   both stay in this file, because the router factory is the only thing that
//!   knows which paths are reserved - the listing's *entries* are still generated,
//!   never hand-maintained.
//! * [`gateway`] is the transport's own layer, outside every route: the four
//!   cross-origin headers the TS host publishes on every response
//!   (`packages/underload/service/src/server/start.ts:415-444`) and the `204` answer
//!   to a CORS preflight. It is a layer rather than a route because a policy that
//!   every response must carry cannot be attached to some of them - and because a
//!   preflight must be answered before the router is consulted, which is what a
//!   route could not do for a path that is only reachable by a *different* verb.
//!
//! What deliberately does NOT live here: the listener, the CLI and shutdown
//! (`src/bin/server.rs`), and any endpoint handler (they are discovered from
//! `examples/endpoints/**` by `build.rs`). Keeping the process/transport half out
//! of the library means an embedder can serve this `Router` on its own listener,
//! and `tests/server_http.rs` can serve it on an ephemeral port, without either
//! inventing a second copy of the route table.
//!
//! This module is part of the native-only library (see `src/lib.rs`); the axum/tokio
//! dependencies are ordinary dependencies in `Cargo.toml`.

use std::path::{Component, Path, PathBuf};

use axum::Json;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::Request;
use axum::http::Method;
use axum::http::header;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

// The wire shape of the endpoint listing, and the browser tool that reads it.
// `metadata` (sibling file) only defines the DTO; the entries themselves are
// generated at crate scope by `build.rs` (`crate::route_metadata`, the companion
// of `crate::register_routes`), so this module owns the route and the type name,
// never a hand-maintained list of endpoints.
mod metadata;
pub use metadata::EndpointMeta;

// The gateway's per-request line (the `Incoming request: [GET] http://host/path` the TS host
// prints at `start.ts:803`). It is a sibling file rather than a few lines in here because it is
// a second contract of the same transport: its mount point is `app_for`, but its parity rules,
// its URL-rebuilding rules and its tests are all its own (see that file's module doc).
mod request_log;
// The one piece of that contract that is observable from OUTSIDE this crate: the exact prefix,
// so the binary's `--help` text and the integration tests can pin the same bytes the layer
// prints instead of restating them. The line builder itself and `log_request` stay private - an
// embedder gets the line by calling `app_for`, which is where the parity rules live.
pub use request_log::REQUEST_LOG_PREFIX;

/// Upper bound on a request body, applied as an axum layer so an oversized payload
/// is refused (413) before any endpoint parses it. The contract endpoints exchange
/// small JSON objects, and a POST body is attacker-controlled, so the cap is a
/// server-side constant rather than something a route can forget to set.
///
/// 1 MiB is comfortably above every documented example body and below anything a
/// caller would reasonably call a "payload"; raise it here (single place) when a
/// real endpoint needs more, never per-route.
///
/// What this cap does NOT cover (T6): it is a [`DefaultBodyLimit`], and that layer
/// only governs extractors that ask for it - `Json`/`Bytes`/`String`/`Form`
/// (`axum-core-0.5.6/src/extract/default_body_limit.rs`, "if an extractor consumes
/// the body directly with `Body::poll_frame`, or similar, the default limit is _not_
/// applied"). The `/v1/harness` bridge takes `Request` and streams the body itself,
/// so it is bounded by the harness's OWN per-route caps (1 MiB, and 16 MiB for
/// `POST /v1/harness/sessions/{id}/tasks`, `src/api/mod.rs::body_limit_for`), exactly
/// as it is on the standalone listener. Widening this constant would therefore change
/// nothing for the harness while silently loosening every JSON endpoint, which is why
/// the layer stays at 1 MiB and `tests/harness_gateway.rs` pins a > 1 MiB body on the
/// harness route answering as a DOMAIN response (404), never as a framework 413.
pub const MAX_REQUEST_BODY_BYTES: usize = 1 << 20;

/// Shared state for every route. Cloneable by design: axum clones it per request
/// handle, so it must stay a cheap handle (paths/`Arc`s), never a workspace full of
/// data.
///
/// `database_root` is the WRITABLE data boundary and is intentionally a different
/// directory from the endpoint *scanner* root: the tree being scanned for route
/// files must never turn into a data directory. The TS entry encodes the same split
/// (`packages/underload/service/server.ts:13-16,34-37`). Paths are never serialised
/// into responses - see the `/v1/services/health` contract - so moving this root
/// cannot change any HTTP-visible value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppState {
    /// Root directory below which endpoints keep their own folders.
    pub database_root: PathBuf,
}

impl AppState {
    /// Construction from anything path-shaped (`&str`, `String`, `&Path`, `PathBuf`),
    /// which is what both the bin (CLI/env string) and tests (repo scratch path)
    /// hand in.
    #[must_use]
    pub fn new(database_root: impl Into<PathBuf>) -> Self {
        Self {
            database_root: database_root.into(),
        }
    }
}

/// Built-in default for [`AppState::database_root`], matching the TS service so both
/// runtimes read and write the same dump directory:
/// `packages/underload/service/server.ts:4,16` resolves
/// `path.join(<repo-root>, '..', 'temporary', 'dump')`, i.e. a sibling OF the repo
/// root. This crate sits at the same depth (`packages/<scope>/<name>`), so the repo
/// root is `../../../` from the manifest and the data root is one level further up.
///
/// `CARGO_MANIFEST_DIR` is baked in at compile time (absolute), which is exactly the
/// "runnable from the repo" behaviour this default is for; a deployed binary passes
/// `--database-root` / `DATABASE_ROOT_ENV` instead. The directory is NOT created
/// here: the root is a boundary, and whoever writes first creates its own folder
/// below it (same policy as the TS `variables: { root }`).
#[must_use]
pub fn default_database_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Four levels up == repo parent (services -> typecrate -> packages -> repo),
    // then the shared `temporary/dump` boundary.
    normalize_lexically(&manifest_dir.join("../../../../temporary/dump"))
}

/// Lexical `.`/`..` resolution with NO filesystem access.
///
/// `Path::canonicalize` would require the target to exist (the dump directory is
/// created lazily by the first writer) and on Windows it also strips the verbatim
/// prefix, which makes logged paths confusing. So paths are cleaned by component
/// instead:
///   * `.` is dropped,
///   * `..` pops a real segment; at the root of an absolute path a further `..` is
///     dropped (`/.. == /`), while a leading `..` on a relative path is kept because
///     there is nothing to resolve it against yet,
///   * everything else (prefix, root, normal) is copied through.
///
/// Public because the CLI (`src/bin/server.rs`) applies the exact same rules to a
/// user-supplied `--database-root`; one implementation means the logged root and the
/// root endpoints derive from can never disagree.
#[must_use]
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                // Only a real segment can be popped; `Prefix`/`RootDir` mean we are
                // already at the top, and an empty `out` means the `..` is relative
                // and must survive.
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::Prefix(_)) | Some(Component::RootDir) => {}
                _ => out.push(component.as_os_str()),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The browser surface (T18): one embedded playground document and the metadata
// listing it reads.
//
// Why embedded rather than served from disk: the executable has no static-file
// mount and no asset directory to get wrong (`AppState.database_root` is a *data*
// boundary, never a document root), so the tool travels inside the binary through
// `include_str!`. rustc tracks each included file as a dependency, so editing an
// asset rebuilds the crate - there is no cache to invalidate and no path that can
// resolve differently on another machine.
//
// Why three files instead of one self-contained document: the response policy
// below grants no `'unsafe-inline'`, and that is only true because the document
// has neither an inline `<script>` nor an inline `<style>`. Splitting the assets is
// what keeps the strict policy honest.
// ---------------------------------------------------------------------------

/// The playground page: `GET /`.
pub const PLAYGROUND_PATH: &str = "/";

/// The playground stylesheet, referenced by the page as `/playground.css`.
pub const PLAYGROUND_STYLES_PATH: &str = "/playground.css";

/// The playground script, referenced by the page as `/playground.js`.
pub const PLAYGROUND_SCRIPT_PATH: &str = "/playground.js";

/// The discovered-endpoint listing: `GET /v1/services/routes`.
pub const ROUTE_METADATA_PATH: &str = "/v1/services/routes";

/// The page document, embedded at compile time (see the block comment above).
const PLAYGROUND_HTML: &str = include_str!("playground.html");

/// The page's stylesheet, embedded at compile time.
const PLAYGROUND_STYLES: &str = include_str!("playground.css");

/// The page's script, embedded at compile time.
const PLAYGROUND_SCRIPT: &str = include_str!("playground.js");

/// A policy written for exactly the three embedded documents and no other origin:
///
/// * `default-src 'self'` - nothing is fetched from a CDN, so the tool works on a
///   LAN with no internet (the whole point of a service playground).
/// * `script-src 'self'` / `style-src 'self'` - no `'unsafe-inline'`: the document
///   carries no inline script or style, and `playground.js` builds the DOM with
///   `createElement`/`textContent` plus class and `data-*` attributes instead of
///   writing `style` attributes. A response body full of markup is therefore
///   displayed as text, never executed.
/// * `connect-src 'self'` - `fetch` cannot leave this origin, which is the
///   server-side half of the same-origin rule the page also enforces with
///   `mode: "same-origin"` and `redirect: "error"`.
/// * `img-src 'self' data:` - only the document's own data-URI favicon needs `data:`.
/// * `object-src 'none'`, `base-uri 'none'`, `form-action 'self'` - no plugin
///   content, no `<base>` element hijacking the relative asset/request paths, and
///   no form navigation off this origin.
///
/// `frame-ancestors` is deliberately absent (with it, `X-Frame-Options`): embedding
/// this tool in another origin's dashboard stays possible.
const PLAYGROUND_CSP: &str = "default-src 'self'; img-src 'self' data:; script-src 'self'; style-src 'self'; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'";

/// `no-store` on every response below, including the assets: they are compiled
/// into the binary, so a cached copy is a *different version of the tool*. Serving
/// a fresh document with a stale cached script is the failure mode this avoids,
/// and a few kilobytes per page load is cheaper than an etag/`Cache-Control` story.
const EMBEDDED_CACHE_CONTROL: &str = "no-store";

/// Renders one embedded document with the shared browser-facing header set.
///
/// The header array is used instead of `Response::builder` because a conversion
/// failure then degrades into a `500` from axum's `IntoResponse` impl
/// (axum-core-0.5.6 `response/into_response_parts.rs:141`) instead of a panic at
/// the first request. Every value here is an ASCII constant, so that path is
/// unreachable in practice; the tests pin the header set byte for byte.
#[must_use]
fn embedded_response(content_type: &'static str, body: &'static str) -> Response {
    let headers = [
        (header::CONTENT_TYPE, content_type),
        (header::CACHE_CONTROL, EMBEDDED_CACHE_CONTROL),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CONTENT_SECURITY_POLICY, PLAYGROUND_CSP),
    ];
    (headers, body).into_response()
}

/// `GET /` - the playground document. `Content-Type` is explicit: the body is
/// `include_str!`d UTF-8, and the charset is what makes non-ASCII summaries safe.
async fn playground_page() -> Response {
    embedded_response("text/html; charset=utf-8", PLAYGROUND_HTML)
}

/// `GET /playground.css` - the document's stylesheet.
async fn playground_stylesheet() -> Response {
    embedded_response("text/css; charset=utf-8", PLAYGROUND_STYLES)
}

/// `GET /playground.js` - the document's script.
async fn playground_script() -> Response {
    embedded_response("text/javascript; charset=utf-8", PLAYGROUND_SCRIPT)
}

/// `GET /v1/services/routes` - the endpoint table the playground navigates by.
///
/// `crate::route_metadata` is generated next to `crate::register_routes` from the
/// same scan, in the same order, so the listing cannot drift from what is
/// actually registered. Builtins (this route and the three documents above) are
/// not part of the generated table, and that is intentional: the listing answers
/// "what endpoints does this build compile in", not "what does the socket accept".
async fn route_metadata_listing() -> Response {
    // The binding names the contract explicitly: one `EndpointMeta` per discovered
    // route, which is also what makes the re-export above load-bearing.
    let routes: Vec<EndpointMeta> = crate::route_metadata();
    // The same header set as the documents (minus `content-type`, which `Json`
    // owns: axum-core inserts `application/json` last, and UTF-8 JSON needs no
    // charset per RFC 8259). A policy is inert on a JSON body, and one shared set
    // is one less thing to reason about for every builtin response.
    let headers = [
        (header::CACHE_CONTROL, EMBEDDED_CACHE_CONTROL),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CONTENT_SECURITY_POLICY, PLAYGROUND_CSP),
    ];
    (headers, Json(routes)).into_response()
}

// ---------------------------------------------------------------------------
// The gateway half of every response (T6): the cross-origin header set the
// `@underload/service` host publishes, plus the preflight answer in front of routing.
//
// TypeScript origin: `packages/underload/service/src/server/start.ts:415-444`, which wraps
// every app (gateway AND worker) with exactly two middlewares:
//
//   * `app.use('*', ...)` registered BEFORE `cors()`, so - onion model - it writes its header
//     AFTER the response exists (`start.ts:434-437`): `Access-Control-Allow-Private-Network:
//     true` therefore lands on cors()'s own `204`, on the gateway's `307` redirect answers, on
//     every endpoint response, and on the media responses that bypass Hono entirely
//     (`src/server/media.ts:86-98` restates the same two values for its raw pipe).
//   * `cors({ origin: '*', allowMethods: ['*'], allowHeaders: ['*'] })` (`start.ts:438`), which
//     answers a CORS preflight with a header-only `204` and otherwise decorates the response.
//
// axum has no hono, so one `from_fn` layer reproduces both halves: `with_gateway_headers` is the
// "after the handler" decoration and `gateway` is the preflight short-circuit. Chrome sends
// `Access-Control-Request-Private-Network: true` on any preflight that targets a private/LAN
// address (a public page fetching `http://192.168.50.109:5000`); without the allow header the
// browser blocks the request before any endpoint runs, which is why the value is set
// unconditionally rather than echoed - the same reasoning the TS comment gives, and the reason a
// cross-origin playground on another device can talk to this server at all.
//
// What is deliberately NOT copied from TS: the `307` worker-redirect tier. That tier exists
// because the TS gateway forks one worker process per endpoint group and answers the gateway's own
// request with a redirect to the worker's port. Here every listener is IN-PROCESS and each one is
// a whole `Router` of its own (`app_for`), so a request is never handed between listeners: a group
// that declares a port is simply mounted on that port's router and is a plain 404 from the
// fallback everywhere else. Process forking is out of scope for the same reason - the extra
// listeners `src/bin/server.rs` binds are tasks inside this one process, not worker processes, and
// nothing in this layer redirects.
//
// Why the preflight test needs `Access-Control-Request-Method`: that header is what makes an
// `OPTIONS` a *preflight* rather than an `OPTIONS` request. Hono's `cors()` short-circuits on the
// pair (method AND the header), and the same condition here is what keeps a plain `OPTIONS`
// routable: `OPTIONS /v1/services/pets` still reaches the router and still answers the router's
// own 405 + `allow` (pinned by `tests/server_http.rs`), and a bare `OPTIONS /v1/harness` still
// reaches the harness dispatcher, which answers its own contract sentence. Short-circuiting every
// `OPTIONS` would have silently retired both behaviours, and every endpoint in this crate answers
// `OPTIONS` only through a fallback - none declares it.
//
// Why a `101` upgrade survives this layer: only `Response::headers_mut()` is touched, and the
// WebSocket handover is carried by the REQUEST's extensions (`hyper::upgrade::on` reads the
// callback hyper attached there), which `Next::run` forwards untouched. Proven over a real socket
// by `tests/harness_gateway.rs`.
// ---------------------------------------------------------------------------

/// `Access-Control-Allow-Private-Network` is not in the `http` crate's header table (it is a
/// Chrome-specified response header that never became an RFC), so its name is spelled once here.
/// `HeaderName::from_static` is a `const fn` (http-1.5.0 `src/header/name.rs:1234`), so the
/// lowercase-normalised name is built at compile time and never re-parsed per request.
const PRIVATE_NETWORK_ALLOW_HEADER: HeaderName =
    HeaderName::from_static("access-control-allow-private-network");

/// The value the three `Access-Control-Allow-*` headers carry: this host answers every origin,
/// every verb and every header, exactly as the TS gateway does (`allowMethods: ['*']`,
/// `allowHeaders: ['*']`).
const GATEWAY_ALLOW_ALL: &str = "*";

/// Adds the gateway's four cross-origin headers to `response`, replacing anything a handler set
/// for them so the advertised policy is the transport's, not one endpoint can quietly narrow.
#[must_use]
fn with_gateway_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static(GATEWAY_ALLOW_ALL),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(GATEWAY_ALLOW_ALL),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(GATEWAY_ALLOW_ALL),
    );
    // The PNA answer is `true` (not an echo): a client that did not ask for it ignores the
    // header, and a client that did must not have to re-negotiate after a redirect.
    headers.insert(
        PRIVATE_NETWORK_ALLOW_HEADER,
        HeaderValue::from_static("true"),
    );

    response
}

/// A CORS preflight, i.e. an `OPTIONS` that declares the verb it wants to send next
/// (`Access-Control-Request-Method`). Hono's `cors()` gates its `204` on the same pair, which is
/// what keeps a plain `OPTIONS` a normal routable request - see the block comment above.
#[must_use]
fn is_cors_preflight(request: &Request) -> bool {
    request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// The outermost layer of [`app`]: a preflight `204` before routing, otherwise the routed
/// response with [`with_gateway_headers`] applied to it.
///
/// `Next`'s default body/state parameters are the right ones here precisely because this layer
/// sits AFTER `.with_state`, so it never has to name `AppState` (the harness route files do).
async fn gateway(request: Request, next: Next) -> Response {
    if is_cors_preflight(&request) {
        // Body-less by definition (`204` means "no body", and the CORS answer is the whole
        // response), so no media type is claimed and no body is drained.
        return with_gateway_headers(StatusCode::NO_CONTENT.into_response());
    }

    with_gateway_headers(next.run(request).await)
}

/// The whole HTTP surface of ONE listener: generated routes + the shared body cap +
/// the gateway header and preflight layer, frozen with `state`.
///
/// `port` selects WHICH listener is being built and is the same value the route files
/// declare with their optional `, port:` trailer (see
/// `src/server/metadata.rs` "What `port` means"): `None` is the default listener,
/// `Some(N)` is the listener for port N. Only the groups whose declared port equals
/// `port` are mounted, which is what makes the user-facing rule true: one API path may
/// be declared twice, once per port, and the two declarations never override each
/// other because they land in two different `Router` values. The group for the *other*
/// port contributes no `.route()` call at all, so a path that is not served here does
/// not exist here (the fallback's plain `404`, never a router `405`).
///
/// `crate::register_routes` is emitted by `build.rs` into `$OUT_DIR/service_routes.rs`
/// and included at crate scope by `src/lib.rs`; each discovered route file contributes
/// its own `register(router, port)` call, which filters its groups by that `port`, so
/// this function never needs to know which endpoints exist or on which ports.
///
/// **The builtin browser surface is mounted on the default listener only** (`port ==
/// None`), and that is a decision rather than an omission: the playground document
/// navigates exclusively through `GET /v1/services/routes`, while the listing answers
/// with every row of the table - rows that on an extra listener mostly belong to a
/// different port. An extra listener exists to serve exactly the API paths that asked
/// for it, so it carries neither the page nor the listing: `/`, `/playground.css`,
/// `/playground.js` and `/v1/services/routes` are plain 404s there. `app` (the default
/// listener) keeps all four.
///
/// Layer order is load-bearing and is identical for every listener: `.layer` only
/// affects routes registered *before* it, so the body limit is applied after
/// `register_routes` (covering every generated route on this listener) and
/// `.with_state` comes last of the state-bearing chain to erase `AppState` from the
/// router type, which is what lets `app_for` return the plain `axum::Router` the bin
/// and the tests both serve. The gateway layer rides on the OUTSIDE of all of it, so
/// no route - generated, builtin or a future fallback - can escape the cross-origin
/// header set, on the default listener or on a declared one.
///
/// One more layer is mounted between those two, on every listener that has routes:
/// [`request_log::log_request`] through `route_layer`, the TS-parity
/// `Incoming request: [METHOD] URL` line (`start.ts:803`). `route_layer` - not `layer` - is
/// what makes that line satisfy the parity rule "only for a request a route actually answered":
/// it wraps the mounted routes and leaves the fallback alone, so an unmatched path stays
/// unlogged, and sitting INSIDE the gateway layer keeps a preflight `204` unlogged as well. The
/// exact rules, each with the axum source that produces it, are in `src/server/request_log.rs`.
///
/// The builtin browser surface is registered in the same position on purpose: the four
/// routes above are added before `.layer(DefaultBodyLimit::max(..))`, so the one body
/// cap covers them too and there is no route outside the limit.
///
/// Nothing here shadows a discovered route silently. The four paths are reserved, and
/// a route file that registers one of them with `GET` on the DEFAULT listener hits
/// axum's duplicate path check while the router is being built (`PathRouter::route` ->
/// `set_node` -> matchit insert, surfaced by `panic_on_err!`, axum-0.8.9
/// `src/routing/{mod.rs:48-55,path_router.rs:141-159}`), i.e. `app_for` panics at
/// startup instead of quietly losing an endpoint. A group that declares its own port
/// lands in a router where the builtins are not mounted at all, so it cannot clash
/// with them - and cannot use those four paths for a tool the playground does not
/// serve.
///
/// No `#[must_use]` here: axum already marks `Router` itself `#[must_use]`
/// (axum-0.8.9 `src/routing/router.rs`), and repeating it is
/// `clippy::double_must_use`, which `npm run lint:strict` denies.
pub fn app_for(port: Option<u16>, state: AppState) -> Router {
    // The generated chain is the ONLY source of API routes, and it filters per group
    // against the listener being built (`src/server/metadata.rs`), so an extra-port
    // router here holds just the groups that declared it.
    let router = crate::register_routes(Router::new(), port);

    // Builtins on the default listener only - see the "builtin browser surface"
    // paragraph above. `else` keeps the chain untouched, so an extra-port router is
    // exactly the generated table plus the shared layers.
    let router = if port.is_none() {
        router
            .route(ROUTE_METADATA_PATH, get(route_metadata_listing))
            .route(PLAYGROUND_PATH, get(playground_page))
            .route(PLAYGROUND_STYLES_PATH, get(playground_stylesheet))
            .route(PLAYGROUND_SCRIPT_PATH, get(playground_script))
    } else {
        router
    };

    let router = router
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state);

    // The per-request line (`src/server/request_log.rs`) rides on the MATCHED routes of THIS
    // listener, and `route_layer` is the axum mechanism that means exactly that:
    // `Router::route_layer` layers `path_router` and leaves both `fallback_router` and
    // `catch_all_fallback` untouched (`axum-0.8.9/src/routing/mod.rs:321-335`), so a request for
    // a path this listener does not mount is answered by the fallback `404` without ever
    // producing a log line - the parity rule the TS host satisfies by logging inside an endpoint
    // handler that only a matched route reaches.
    //
    // Two placements are load-bearing and both are visible here:
    //
    //   * it is INSIDE the `gateway` layer below, so a CORS preflight - answered by `gateway`
    //     before the router is consulted - produces no line at all, while a bare `OPTIONS` is
    //     still routed and still logged;
    //   * it is OUTSIDE `DefaultBodyLimit` (added after it, therefore the outer one), so the
    //     request path stays `gateway -> log -> body limit -> route` and the pinned `413`/`405`/
    //     `404` answers of `tests/server_http.rs` come from the same chain they came from before:
    //     `log_request` reads only the method and the URI, and hands the request and the response
    //     to `Next` untouched.
    //
    // `has_routes` is the guard `route_layer` requires: a router built for a port nothing
    // declared mounts ZERO routes, and axum panics on a layer with nothing to wrap
    // (`axum-0.8.9/src/routing/path_router.rs:319-324`) - under `panic = "abort"` that panic
    // would be the start of the process. A router with no routes answers every request with the
    // fallback `404`, i.e. the one case this layer must not log anyway, so skipping the mount
    // changes nothing observable.
    let router = if router.has_routes() {
        router.route_layer(middleware::from_fn(request_log::log_request))
    } else {
        router
    };

    router
        // Outermost, so a preflight is answered before the router is consulted AND
        // every response - including a `101` upgrade and a bare 404 - carries the
        // same four headers.
        .layer(middleware::from_fn(gateway))
}

/// The whole HTTP surface of the DEFAULT listener - literally [`app_for(None, state)`].
///
/// The signature is unchanged from before route files could declare ports, and so is
/// everything it builds: the unported groups (the ones written without `, port:`), the
/// four builtins, the body cap, the state and the gateway layer, in the same order.
/// That is what keeps the README's claim true - a test that serves `app(state)` and the
/// executable's default listener run the same router - while the extra listeners the bin
/// binds are `app_for(Some(port), state)` beside it.
pub fn app(state: AppState) -> Router {
    app_for(None, state)
}

/// Every port the discovered route table declares with a `, port:` trailer, sorted and
/// de-duplicated - i.e. the set of EXTRA listeners a process has to bind next to its
/// default one.
///
/// This is derived, never configured: the route table is the only thing that knows
/// which ports are in use, so `src/bin/server.rs` binds exactly this set and cannot
/// drift from it (a declared port with no listener would 404 forever, and a listener
/// with no declared routes would serve an empty router). The default listener is NOT
/// part of the answer - its port is the caller's business (`--addr`/`--port`/env in the
/// bin, `0.0.0.0:8000` there) - which is also why the value is `Option<u16>`-shaped
/// metadata rather than a `u16` here.
///
/// A build whose route files declare no port answers with an empty `Vec`, which is the
/// case the bin uses to stay on its single listener.
#[must_use]
pub fn declared_ports() -> Vec<u16> {
    ports_from_metadata(&crate::route_metadata())
}

/// The pure half of [`declared_ports`]: the distinct `Some(port)` values of `routes`,
/// sorted and de-duplicated. Private so the public surface stays one function, while
/// the rules (duplicates collapse, order is ascending, an all-`None` table and an empty
/// table both stay empty) are still unit-testable without the discovered tree.
#[must_use]
fn ports_from_metadata(routes: &[EndpointMeta]) -> Vec<u16> {
    let mut ports: Vec<u16> = routes.iter().filter_map(|route| route.port).collect();
    // `sort_unstable` + `dedup` instead of a `BTreeSet`: the row count is tiny, this is
    // called at startup (not per request), and the result is a plain `Vec` either way.
    ports.sort_unstable();
    ports.dedup();
    ports
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the gateway tests below need these two (`app()` itself never names a body type).
    use axum::body::Body;
    use axum::http::HeaderMap;

    /// The normaliser is the part of the default that can silently go wrong (a wrong
    /// number of `..` lands the data root inside the repo, which is the exact failure
    /// the TS comment warns about), so its rules are pinned exactly.
    #[test]
    fn normalize_lexically_resolves_dot_and_dotdot_exactly() {
        assert_eq!(
            normalize_lexically(Path::new("/a/b/../../c/d")),
            PathBuf::from("/c/d")
        );
        // `.` contributes nothing, `..` pops only real segments.
        assert_eq!(
            normalize_lexically(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        // Past the root `..` stops rather than producing `/../x`.
        assert_eq!(
            normalize_lexically(Path::new("/a/b/../../../c")),
            PathBuf::from("/c")
        );
        // Relative paths keep their leading `..` (nothing to resolve it against).
        assert_eq!(
            normalize_lexically(Path::new("../y")),
            PathBuf::from("../y")
        );
        assert_eq!(normalize_lexically(Path::new("x/../y")), PathBuf::from("y"));
        assert_eq!(
            normalize_lexically(Path::new("./a/./b")),
            PathBuf::from("a/b")
        );
    }

    #[test]
    fn default_database_root_is_the_repo_parent_temporary_dump() {
        let root = default_database_root();
        // Component-wise suffix check: separator spelling differs per platform, the
        // segment sequence must not.
        assert!(root.ends_with(Path::new("temporary").join("dump")));
        // Already normalised: no `..` left anywhere.
        assert!(
            !root
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        );
        // The data root is outside the repo: the manifest lives at
        // `<repo>/packages/<scope>/<name>`, so its repo root is 3 levels up and the
        // dump root is the sibling of THAT (4 up), never the scanner root.
        // Pinned as the exact path (not just `parent() == repo_parent`, which would
        // pass for `<repo-parent>/dump` too and miss the extra `temporary` level the
        // TS entry uses, `packages/underload/service/server.ts:16`).
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_parent = manifest
            .parent() // -> packages/<scope>
            .and_then(|p| p.parent()) // -> packages
            .and_then(|p| p.parent()) // -> repo root
            .and_then(|p| p.parent()) // -> repo parent
            .expect("the manifest sits at <repo>/packages/<scope>/<name>");
        assert_eq!(root, repo_parent.join("temporary").join("dump"));
    }

    #[test]
    fn app_state_is_a_cloneable_path_handle() {
        // axum requires the state to be Clone; the test/bin paths rely on being able
        // to hand the same value to `app()` and keep reading their own copy.
        let state = AppState::new("/tmp/whatever");
        let clone = state.clone();
        assert_eq!(clone, state);
        assert_eq!(state.database_root, PathBuf::from("/tmp/whatever"));
        // `new` accepts the shapes the callers actually have.
        assert_eq!(
            AppState::new(String::from("/a")).database_root,
            PathBuf::from("/a")
        );
        assert_eq!(
            AppState::new(Path::new("/b")).database_root,
            PathBuf::from("/b")
        );
    }

    /// The preflight test is what separates a browser handshake from an ordinary `OPTIONS`
    /// request, so BOTH halves are pinned: the verb alone must not short-circuit the router
    /// (that would retire the documented 405 + `allow` answers), and the header alone must not
    /// either (a `GET` may carry `Access-Control-Request-Method` for any reason).
    #[test]
    fn only_an_options_request_that_declares_its_method_is_a_preflight() {
        // A real Chrome PNA preflight: OPTIONS + the verb it wants + the private-network ask.
        let preflight = Request::builder()
            .method(Method::OPTIONS)
            .uri("/v1/harness")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header("access-control-request-private-network", "true")
            .body(Body::empty())
            .expect("a request the builder can represent");
        assert!(is_cors_preflight(&preflight));

        // The same verb with no declared method is a plain request: it must be routed.
        let bare = Request::builder()
            .method(Method::OPTIONS)
            .uri("/v1/harness")
            .body(Body::empty())
            .expect("a request the builder can represent");
        assert!(!is_cors_preflight(&bare));

        // The same header on another verb is not a handshake either.
        let with_header = Request::builder()
            .method(Method::GET)
            .uri("/v1/harness")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .body(Body::empty())
            .expect("a request the builder can represent");
        assert!(!is_cors_preflight(&with_header));
    }

    /// The published policy, byte for byte, and the two things the decorator must not do: change
    /// the status it is handed, or leave an endpoint's narrower `Access-Control-Allow-Origin`
    /// standing under the gateway's (`insert`, not `entry`, is what makes the transport win).
    #[test]
    fn the_gateway_layer_publishes_the_cross_origin_headers_it_promises() {
        let mut authored = HeaderMap::new();
        authored.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("http://example.invalid"),
        );
        let response = with_gateway_headers((authored, "a body").into_response());
        let headers = response.headers();

        for name in [
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            header::ACCESS_CONTROL_ALLOW_METHODS,
            header::ACCESS_CONTROL_ALLOW_HEADERS,
        ] {
            assert_eq!(
                headers.get(&name).and_then(|value| value.to_str().ok()),
                Some(GATEWAY_ALLOW_ALL),
                "{name} must be the wildcard the TS host publishes"
            );
        }
        assert_eq!(
            headers
                .get(PRIVATE_NETWORK_ALLOW_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        // Exactly one copy of each, whatever the handler already sent.
        assert_eq!(
            headers
                .get_all(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .iter()
                .count(),
            1,
            "a duplicated CORS header is a browser-level rejection"
        );
        assert_eq!(response.status(), StatusCode::OK);
    }

    // -------------------------------------------------------------------
    // The port-aware router factory (T7).
    //
    // These go over a real TCP socket, the way `tests/server_http.rs` does: every
    // question below is a DISPATCH question ("does the router built for 8010 really
    // serve /v1/services/health while the default router keeps serving it?"), and
    // this crate has no in-process dispatch path - `tower` is not one of its
    // dependencies, so `Router::oneshot` is not even nameable here.
    // -------------------------------------------------------------------

    /// `GET /v1/services/health`'s body, fixed by the endpoint contract
    /// (`examples/endpoints/health/health-core.rs` fills exactly these two fields from
    /// Cargo metadata, and `tests/server_http.rs` pins the same bytes).
    const HEALTH_BODY: &str = r#"{"service":"typecrate-services","version":"1.0.2"}"#;

    /// The one API path the tree declares TWICE, once per listener
    /// (`examples/endpoints/health/health-route.rs`) - the pair this feature exists for.
    const SHARED_PATH: &str = "/v1/services/health";

    /// The ports the tree declares (in ascending order, the order `declared_ports()`
    /// promises): 8010 is the `examples/endpoints/health/health-route.rs` group, 8800 is the
    /// four `/providers/private/v1` groups of the foreign route file
    /// `packages/typecrate/provider/endpoint/provider/provider-route.rs`. `AN_UNDECLARED_PORT`
    /// is a port NOTHING declares: 8011 appears in a `service_routes!` probe inside
    /// `src/server/metadata.rs`'s own tests, which is not part of the discovered table, so
    /// `declared_ports()` cannot report it.
    const DECLARED_PORT: u16 = 8010;
    const PROVIDER_PORT: u16 = 8800;
    const AN_UNDECLARED_PORT: u16 = 8011;

    /// A data root that never has to exist: every request dialed below is a read, and the
    /// path lives under the repo `Temp/` scratch area (AGENTS.md) rather than the shared
    /// `temporary/dump` boundary.
    fn scratch_state() -> AppState {
        AppState::new(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../Temp/opencode/server-ports"),
        )
    }

    /// Serves `router` on an ephemeral loopback listener, returning its base URL plus the
    /// task handle, so one test can hold two listeners at the same time.
    async fn serve_ephemeral(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral loopback listener binds");
        let address = listener
            .local_addr()
            .expect("a bound listener reports its address");
        let handle = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("the ephemeral listener serves to completion");
        });
        (format!("http://{address}"), handle)
    }

    /// A client that never parks an idle connection: `axum::serve` finishes one connection
    /// before accepting the next, so a keep-alive connection sitting in the pool in front of
    /// the accept loop would serialise - or stall - the rest of a test.
    fn http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .expect("a client with no idle pool builds")
    }

    /// `GET <base><path>` -> the status code, the observable half of every port assertion.
    async fn get_status(client: &reqwest::Client, base: &str, path: &str) -> StatusCode {
        client
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap_or_else(|err| panic!("GET {base}{path} reaches the listener: {err}"))
            .status()
    }

    /// The pure half of the port derivation, on synthetic rows: an empty table and an
    /// all-`None` table both answer "no extra listener", and declared ports come back
    /// ascending and once each whatever the listing order is (the bin binds in this order,
    /// and two groups sharing one port must bind ONE listener).
    #[test]
    fn the_port_helper_sorts_dedupes_and_stays_empty_without_a_declaration() {
        let row = |port: Option<u16>| EndpointMeta {
            method: "GET",
            path: "/probe",
            summary: "A probe row",
            port,
        };
        let none: Vec<u16> = Vec::new();
        assert_eq!(ports_from_metadata(&[]), none);
        assert_eq!(ports_from_metadata(&[row(None), row(None)]), none);
        assert_eq!(
            ports_from_metadata(&[
                row(Some(9000)),
                row(None),
                row(Some(8010)),
                row(Some(9000)),
                row(Some(8010)),
            ]),
            vec![8010, 9000]
        );
    }

    /// AC3: `declared_ports()` is derived from the generated listing, not configured - the
    /// exact answer for this tree today, plus the two invariants that make it safe to bind
    /// from: every ported row's port is in it, and the default listener is not.
    #[test]
    fn declared_ports_is_exactly_the_set_of_ports_the_route_tree_declares() {
        // Today's tree declares exactly two ports: the `port: 8010` group of
        // `examples/endpoints/health/health-route.rs`, and the four `port: 8800` groups of the
        // foreign route file `packages/typecrate/provider/endpoint/provider/provider-route.rs`.
        // Adding or removing a `, port:` group is therefore an explicit edit here, not a silent
        // drift. Ascending, because that is the order `declared_ports()` promises the bin binds.
        assert_eq!(declared_ports(), vec![DECLARED_PORT, PROVIDER_PORT]);
        // Derived, and from the same table `GET /v1/services/routes` serialises.
        let rows = crate::route_metadata();
        assert_eq!(declared_ports(), ports_from_metadata(&rows));
        // Exactly one row declares 8010 and exactly four declare 8800 (the provider mount lists
        // one row per group); every other row belongs to the default listener, so nothing is
        // served on a listener `declared_ports()` did not report.
        assert_eq!(
            rows.iter()
                .filter(|row| row.port == Some(DECLARED_PORT))
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.port == Some(PROVIDER_PORT))
                .count(),
            4
        );
        assert_eq!(
            rows.iter().filter(|row| row.port.is_none()).count(),
            rows.len() - 5
        );
    }

    /// AC1: one API path declared on two ports is served by two INDEPENDENT listeners.
    /// Neither declaration overrides the other, and a third listener for a port nobody
    /// declares does not serve the path at all.
    #[tokio::test]
    async fn the_same_api_path_declared_on_two_ports_is_served_on_both_listeners() {
        let state = scratch_state();
        let default = serve_ephemeral(app_for(None, state.clone())).await;
        let lan = serve_ephemeral(app_for(Some(DECLARED_PORT), state.clone())).await;
        let undeclared = serve_ephemeral(app_for(Some(AN_UNDECLARED_PORT), state)).await;
        let client = http_client();

        // The shared path answers on BOTH listeners with the same contract body: two
        // separate `Router` values, each mounted with its own group.
        for base in [&default.0, &lan.0] {
            let response = client
                .get(format!("{base}{SHARED_PATH}"))
                .send()
                .await
                .unwrap_or_else(|err| panic!("GET {base}{SHARED_PATH} is served: {err}"));
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{base} serves {SHARED_PATH}"
            );
            assert_eq!(response.text().await.expect("a body"), HEALTH_BODY);
        }

        // A port no route file declares is an empty API surface (see the next test for
        // the 404-not-405 detail), so the two real listeners above are not one router.
        assert_eq!(
            get_status(&client, &undeclared.0, SHARED_PATH).await,
            StatusCode::NOT_FOUND
        );
        // And they are distinguishable in the other direction too: the playground is the
        // default listener's, so the declared listener never answers for it.
        assert_eq!(
            get_status(&client, &default.0, PLAYGROUND_PATH).await,
            StatusCode::OK
        );
        assert_eq!(
            get_status(&client, &lan.0, PLAYGROUND_PATH).await,
            StatusCode::NOT_FOUND
        );

        for handle in [default.1, lan.1, undeclared.1] {
            handle.abort();
        }
    }

    /// What an extra router that matches nothing looks like, and why the answer is the
    /// fallback's `404` with no `allow` header: a group whose port does not match
    /// contributes no `.route()` call at all, so the path simply does not exist there
    /// (`src/server/metadata.rs` "What `port` means"), and the builtins are never mounted
    /// on an extra listener at all.
    #[tokio::test]
    async fn a_router_for_a_port_nobody_declares_serves_nothing_but_the_layers() {
        let (base, handle) =
            serve_ephemeral(app_for(Some(AN_UNDECLARED_PORT), scratch_state())).await;
        let client = http_client();

        let response = client
            .get(format!("{base}{SHARED_PATH}"))
            .send()
            .await
            .expect("the listener answers");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            !response.headers().contains_key(header::ALLOW),
            "a path that is not mounted here is a 404, never a router 405 with a verb list"
        );

        for path in [
            PLAYGROUND_PATH,
            PLAYGROUND_STYLES_PATH,
            PLAYGROUND_SCRIPT_PATH,
            ROUTE_METADATA_PATH,
        ] {
            assert_eq!(
                get_status(&client, &base, path).await,
                StatusCode::NOT_FOUND,
                "{path} belongs to the default listener only"
            );
        }

        handle.abort();
    }

    /// AC2: `app(state)` is `app_for(None, state)` - every generated GET behaves
    /// identically through both factories, and the four builtins are still there.
    #[tokio::test]
    async fn app_is_the_default_listener_router_for_every_route_and_the_four_builtins() {
        let state = scratch_state();
        let factory = serve_ephemeral(app(state.clone())).await;
        let same = serve_ephemeral(app_for(None, state)).await;
        let client = http_client();

        // Every GET the generated table lists, dialed through both routers. Only GETs are
        // dialed: the write samples would mutate the store between the two calls and
        // compare a create against a second create.
        let rows = crate::route_metadata();
        let mut dialed: Vec<&str> = Vec::new();
        for row in rows.iter().filter(|row| row.method == "GET") {
            let through_app = get_status(&client, &factory.0, row.path).await;
            let through_app_for = get_status(&client, &same.0, row.path).await;
            assert_eq!(
                through_app, through_app_for,
                "GET {} must not depend on which factory built the default router",
                row.path
            );
            dialed.push(row.path);
        }
        // The loop actually ran over the table (a listing that stopped containing the
        // shared path would make every assertion above vacuous).
        assert!(
            dialed.contains(&SHARED_PATH),
            "the listing must contain {SHARED_PATH}"
        );
        assert_eq!(
            dialed.len(),
            rows.iter().filter(|row| row.method == "GET").count()
        );

        // The four framework builtins, unchanged by the port work.
        for path in [
            PLAYGROUND_PATH,
            PLAYGROUND_STYLES_PATH,
            PLAYGROUND_SCRIPT_PATH,
            ROUTE_METADATA_PATH,
        ] {
            assert_eq!(
                get_status(&client, &factory.0, path).await,
                StatusCode::OK,
                "{path} is still served on the default listener"
            );
        }

        // The listing on the default listener is the generated metadata verbatim, and it
        // carries exactly one row with a `port` key - the playground's only hint about
        // which listener a row belongs to.
        let response = client
            .get(format!("{}{ROUTE_METADATA_PATH}", factory.0))
            .send()
            .await
            .expect("the listing is served");
        assert_eq!(response.status(), StatusCode::OK);
        let listing: serde_json::Value = response.json().await.expect("the listing is JSON");
        assert_eq!(
            listing,
            serde_json::to_value(rows).expect("endpoint metadata is serialisable")
        );
        assert_eq!(
            listing
                .as_array()
                .expect("a JSON array")
                .iter()
                .filter(|row| row.get("port") == Some(&serde_json::json!(DECLARED_PORT)))
                .count(),
            1,
            "one declared listener, one `port` key"
        );

        for handle in [factory.1, same.1] {
            handle.abort();
        }
    }

    /// The gateway layer is outside EVERY listener, declared or not: the four cross-origin
    /// headers ride on a 200 from an extra port, and a preflight is still answered before
    /// routing there.
    #[tokio::test]
    async fn an_extra_listener_still_answers_through_the_gateway_layer() {
        let (base, handle) = serve_ephemeral(app_for(Some(DECLARED_PORT), scratch_state())).await;
        let client = http_client();

        let response = client
            .get(format!("{base}{SHARED_PATH}"))
            .send()
            .await
            .expect("the extra listener answers");
        assert_eq!(response.status(), StatusCode::OK);
        for name in [
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            header::ACCESS_CONTROL_ALLOW_METHODS,
            header::ACCESS_CONTROL_ALLOW_HEADERS,
        ] {
            assert_eq!(
                response
                    .headers()
                    .get(&name)
                    .and_then(|value| value.to_str().ok()),
                Some(GATEWAY_ALLOW_ALL),
                "{name} is the transport's on every listener"
            );
        }
        assert_eq!(
            response
                .headers()
                .get(PRIVATE_NETWORK_ALLOW_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );

        let preflight = client
            .request(reqwest::Method::OPTIONS, format!("{base}{SHARED_PATH}"))
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .send()
            .await
            .expect("the extra listener answers a preflight");
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);

        handle.abort();
    }
}
