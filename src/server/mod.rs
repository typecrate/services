//! The native HTTP runtime: shared state + the axum `Router` factory.
//!
//! What lives here and why:
//!
//! * [`AppState`] is the single value every generated handler receives through
//!   axum's `State` extractor. It is cheap to clone (axum requires `Clone` on the
//!   state type) and it is the ONLY place the process learns where writable data
//!   goes - handlers get the root, they never re-derive it from `cwd`.
//! * [`app`] hands that state to the build-time generated route table
//!   (`crate::register_routes`, see `src/lib.rs`), so the router a test builds and
//!   the router the executable serves are literally the same function.
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
//! Compiled only for non-wasm targets (gate in `src/lib.rs`); the axum/tokio
//! dependencies are target-gated in `Cargo.toml` the same way.

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
// because the TS gateway forks one worker process per endpoint group; this router serves
// everything in-process, so there is nothing to redirect to and no second listener to keep in
// sync (the bind defaults are unchanged by T6).
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

/// The whole HTTP surface: generated routes + the shared body cap + the gateway header and
/// preflight layer, frozen with `state`.
///
/// `crate::register_routes` is emitted by `build.rs` into
/// `$OUT_DIR/service_routes.rs` and included at crate scope by `src/lib.rs`; each
/// discovered route file contributes its own `register(router)` call, so this
/// function never needs to know which endpoints exist.
///
/// Layer order is load-bearing: `.layer` only affects routes registered *before*
/// it, so the body limit is applied after `register_routes` (covering every
/// generated route) and `.with_state` comes last of the state-bearing chain to
/// erase `AppState` from the router type, which is what lets `app()` return the
/// plain `axum::Router` the bin and the tests both serve. The gateway layer rides
/// on the OUTSIDE of all of it, so no route - generated, builtin or a future
/// fallback - can escape the cross-origin header set.
///
/// The builtin browser surface is registered in the same position on purpose: the
/// four routes above are added before `.layer(DefaultBodyLimit::max(..))`, so the
/// one body cap covers them too and there is no route outside the limit.
///
/// Nothing here shadows a discovered route silently. The four paths are reserved,
/// and a route file that registers one of them with `GET` hits axum's duplicate
/// path check while the router is being built (`PathRouter::route` -> `set_node`
/// -> matchit insert, surfaced by `panic_on_err!`, axum-0.8.9
/// `src/routing/{mod.rs:48-55,path_router.rs:141-159}`), i.e. `app()` panics at
/// startup instead of quietly losing an endpoint.
///
/// No `#[must_use]` here: axum already marks `Router` itself `#[must_use]`
/// (axum-0.8.9 `src/routing/router.rs`), and repeating it is
/// `clippy::double_must_use`, which `npm run lint:strict` denies.
pub fn app(state: AppState) -> Router {
    crate::register_routes(Router::new())
        .route(ROUTE_METADATA_PATH, get(route_metadata_listing))
        .route(PLAYGROUND_PATH, get(playground_page))
        .route(PLAYGROUND_STYLES_PATH, get(playground_stylesheet))
        .route(PLAYGROUND_SCRIPT_PATH, get(playground_script))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state)
        // Outermost, so a preflight is answered before the router is consulted AND
        // every response - including a `101` upgrade and a bare 404 - carries the
        // same four headers.
        .layer(middleware::from_fn(gateway))
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
}
