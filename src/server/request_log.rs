//! The gateway's per-request line - the one log line the TypeScript host prints and this
//! server used not to.
//!
//! The reference is `packages/underload/service/src/server/start.ts:801-803`, the first
//! statement of the handler every discovered endpoint is wrapped in:
//!
//! ```ts
//! app.all(config.route, async (context) => {
//!     console.log(`Incoming request: [${context.req.method}] ${context.req.url}`);
//! ```
//!
//! so the line is stdout, untagged, untimestamped, and printed BEFORE the handler runs. The
//! known-missing behaviour is recorded in
//! `packages/underload/harness/docs/issues/minor-relay-gateway-silence-adds-no-per-request-provider-log.md`:
//! it belongs to the GATEWAY layer (this file), never to the provider relay path, and this file
//! is exactly that layer - one line per request, on every listener, whoever ends up answering.
//!
//! # Where the line goes, and why that is the hard part
//!
//! [`log_request`] is mounted with `Router::route_layer` inside
//! [`crate::server::app_for`], and `route_layer` is the axum mechanism that produces the
//! parity rule the issue describes:
//!
//! * **it runs only when a route matched** - axum's own words
//!   (`axum-0.8.9/src/docs/routing/route_layer.md`): "will only run if the request matches a
//!   route ... works similarly to [`Router::layer`] except the middleware will only run if the
//!   request matches a route". Concretely, `Router::route_layer`
//!   (`axum-0.8.9/src/routing/mod.rs:321-335`) layers `path_router` and leaves BOTH
//!   `fallback_router` and `catch_all_fallback` alone, which is what keeps an unmatched path -
//!   answered by the catch-all `404` - unlogged;
//! * **a CORS preflight is not logged**, because the preflight short-circuit
//!   ([`crate::server::gateway`]) is a `.layer` on the OUTSIDE of this one and returns its `204`
//!   before the router is consulted at all;
//! * a path that IS mounted but not for that verb still logs: `MethodRouter::layer`
//!   (`axum-0.8.9/src/routing/method_routing.rs:967-993`) layers that path's own verb fallback
//!   too, so the `405` + `allow` answer is produced inside the wrapped route. That is the
//!   parity-correct side to be on: the TypeScript logs before its handler runs, and its
//!   `app.all('*')` middleware runs for a verb the endpoint does not declare;
//! * **a handler's own `404` is logged** - the route matched, and the TypeScript prints the
//!   line for it too, because its log statement is the handler's first statement rather than a
//!   post-response hook;
//! * a WebSocket upgrade logs **once**: the upgrade is one HTTP request through the router and
//!   the frames that follow never re-enter it. This row is a Rust-side decision rather than
//!   parity - the TypeScript host routes upgrades on the raw `http.Server` `upgrade` event, with
//!   "no Hono handler, no middleware" (`packages/underload/service/src/server/start.ts:955-958`,
//!   `968-989`), so its `console.log` never fires for a handshake;
//!
//! `route_layer` panics when the router has no routes at all
//! (`axum-0.8.9/src/routing/path_router.rs:319-324`), which is a real shape here: a router built
//! for a port nothing declared (`app_for(Some(8011), ..)`) mounts zero routes. `app_for`
//! therefore guards the mount with `Router::has_routes()` - a router with no routes answers
//! every request with the fallback `404`, so there is nothing it could log anyway.
//!
//! # Why the line goes out through `tracing` rather than `println!`
//!
//! One call, one effect: `src/bin/server.rs` installs the subscriber either way, and the
//! subscriber renders this target verbatim with no level in front of it
//! (`crate::logging::REQUEST_TARGET`), so the bytes on stdout are the same as the `console.log`
//! they are matching. What `tracing` adds is the two things `println!` cannot do here: an
//! embedder can route the line wherever it likes, and a write failure (a closed stdout) is
//! dropped instead of panicking - which under `panic = "abort"` would otherwise take the
//! process down over a log line.

use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;

use crate::logging::REQUEST_TARGET;

/// The exact opening of every line, spelled once so the tests, the README and `HELP` all
/// describe the same bytes.
pub const REQUEST_LOG_PREFIX: &str = "Incoming request: ";

/// The scheme every rebuilt URL carries. This server has exactly one kind of listener - a plain
/// `tokio::net::TcpListener` served by `axum::serve`, with no TLS and no proxy tier in front of
/// it - so `http` is a fact here rather than the guess the TypeScript's `x-forwarded-proto`
/// handling has to make.
const REQUEST_LOG_SCHEME: &str = "http";

/// The line for one request: `Incoming request: [<METHOD>] <URL as this server saw it>`.
///
/// Pure, and it never panics: `profile.release` sets `panic = "abort"` (`Cargo.toml:129-134`),
/// so this runs on every request with no `unwrap` in it - the method is read through `Method`'s
/// own `Display`-backed `as_str` (an extension method is ASCII by construction) and the URI is
/// read through [`axum::http::Uri`]'s accessors instead of by parsing anything again.
#[must_use]
pub fn request_log_line(request: &Request) -> String {
    format!(
        "{REQUEST_LOG_PREFIX}[{}] {}",
        request.method().as_str(),
        requested_url(request)
    )
}

/// The request target rebuilt the way the TypeScript's `context.req.url` reads: scheme, the
/// authority the client named, and the path with its query.
///
/// Three shapes are possible and each has its own honest answer:
///
/// * **absolute-form** (`GET http://host:8010/v1/x HTTP/1.1`, what a proxy sends; for HTTP/2
///   hyper fills `authority` from `:authority`) - the URI already carries the whole URL, so it
///   is printed as parsed rather than rebuilt from a `Host` header the request does not have;
/// * **origin-form** (`GET /v1/x?a=b`, the ordinary case) - the authority is the `Host` header,
///   the only place an origin-form HTTP/1 request states it, and that is exactly what
///   `context.req.url` uses too;
/// * **no usable authority at all** (an HTTP/1.0 request with no `Host`, or a non-UTF-8 one, or
///   the `OPTIONS *` target) - the line carries the request target verbatim. Inventing a host
///   here would print a URL the client never asked for, and the point of the line is what
///   arrived.
///
/// This is the FORMATTER, and it is what the unit tests below pin: a shape only reaches a
/// terminal if hyper accepted the request line AND a route matched (see [`log_request`]), and
/// the exotic targets here mostly fail one of those first - an asterisk target matches no route
/// and an authority-form target (`CONNECT host:port`) never matches either, so both are answered
/// by the fallback `404` with no line at all.
fn requested_url(request: &Request) -> String {
    let uri = request.uri();

    // Absolute-form (`GET http://host/path`): `Display` renders the whole URL when the URI
    // carries both a scheme and an authority path. The one OTHER shape with an authority is
    // authority-form (`CONNECT host:port HTTP/1.1`), and there the same `Display` prints just
    // that bare `host:port` - still the target as it arrived, and the case stays theoretical
    // here because an authority-form target matches no route, so the request is answered by the
    // fallback `404` before this line could be emitted.
    if uri.authority().is_some() {
        return uri.to_string();
    }

    let target = uri.path_and_query().map_or("", |path| path.as_str());
    if !target.starts_with('/') {
        return target.to_owned();
    }

    match request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
    {
        // `to_str()` covers the only failure a `Host` can be: non-ASCII bytes. It is read as
        // "no usable authority" rather than guessed at from the socket, because the socket's
        // bind address (`0.0.0.0:8000`) is not a URL the client dialed.
        Some(host) if !host.is_empty() => format!("{REQUEST_LOG_SCHEME}://{host}{target}"),
        _ => target.to_owned(),
    }
}

/// The layer: log the arrival, then run the matched route.
///
/// The line is built and emitted BEFORE `next.run` - the position of the TypeScript's
/// `console.log` - so a handler that hangs, panics or answers `404` has all produced its line
/// already, and so the order of the two lines in a terminal is "request, then whatever the
/// request logged".
pub async fn log_request(request: Request, next: Next) -> Response {
    tracing::info!(
        target: REQUEST_TARGET,
        "{}",
        request_log_line(&request)
    );

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderValue, Method};

    /// A request shaped the way hyper hands one over: `scheme`/`authority` are only present for
    /// an absolute-form target, and `Host` is a header like any other.
    fn request(method: Method, uri: &str, host: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        builder
            .body(Body::empty())
            .expect("a request the builder can represent")
    }

    /// The exact contract of the line: the prefix, the bracketed verb, a single space, the URL.
    #[test]
    fn an_origin_form_request_becomes_the_absolute_url_the_client_asked_for() {
        // The canonical case from the parity reference, byte for byte.
        assert_eq!(
            request_log_line(&request(
                Method::GET,
                "/v1/services/health",
                Some("127.0.0.1:8000"),
            )),
            "Incoming request: [GET] http://127.0.0.1:8000/v1/services/health"
        );
        // The query is part of the URL, exactly as `context.req.url` keeps it.
        assert_eq!(
            request_log_line(&request(
                Method::POST,
                "/v1/services/echo?prefix=hi",
                Some("192.168.1.20:8000"),
            )),
            "Incoming request: [POST] http://192.168.1.20:8000/v1/services/echo?prefix=hi"
        );
        // A declared listener is reported with the port the client dialed, because that is the
        // authority the `Host` header carries.
        assert_eq!(
            request_log_line(&request(
                Method::GET,
                "/v1/services/health",
                Some("127.0.0.1:8010"),
            )),
            "Incoming request: [GET] http://127.0.0.1:8010/v1/services/health"
        );
        // A percent-encoded path is printed as it arrived: the URI is never decoded here.
        assert_eq!(
            request_log_line(&request(
                Method::GET,
                "/v1/services/echo/a%20b%20c",
                Some("127.0.0.1:8000"),
            )),
            "Incoming request: [GET] http://127.0.0.1:8000/v1/services/echo/a%20b%20c"
        );
        // A bracketed IPv6 authority survives verbatim, as it does in the banner.
        assert_eq!(
            request_log_line(&request(Method::GET, "/x", Some("[::1]:8000"))),
            "Incoming request: [GET] http://[::1]:8000/x"
        );
    }

    /// An absolute-form target already names the whole URL, so it is printed as parsed and no
    /// `Host` header is invented for it.
    #[test]
    fn an_absolute_form_target_is_printed_as_it_arrived() {
        assert_eq!(
            request_log_line(&request(
                Method::GET,
                "http://127.0.0.1:8010/v1/services/health",
                None,
            )),
            "Incoming request: [GET] http://127.0.0.1:8010/v1/services/health"
        );
        // A non-`http` scheme in the target is kept too: this server is not the one that decided
        // the scheme, the proxy in front of it did.
        assert_eq!(
            request_log_line(&request(
                Method::POST,
                "https://relay.example/v1/x?a=1",
                Some("ignored-host:8000"),
            )),
            "Incoming request: [POST] https://relay.example/v1/x?a=1"
        );
    }

    /// The shapes with no authority to name: the target stands on its own, and nothing is
    /// guessed - while the verb is still printed, because that is the half the line is for.
    ///
    /// These pin [`request_log_line`] as a formatter. Whether such a request ever reaches a
    /// terminal is a different question (see the `requested_url` doc): an asterisk target matches
    /// no route, so the server answers it with the fallback `404` and prints NOTHING - the line
    /// below is what the formatter would produce if a route did match.
    #[test]
    fn a_request_without_a_usable_authority_renders_its_request_target() {
        assert_eq!(
            request_log_line(&request(Method::GET, "/v1/services/health", None)),
            "Incoming request: [GET] /v1/services/health"
        );
        // An empty `Host` header is "no authority", not `http:///path`.
        assert_eq!(
            request_log_line(&request(Method::DELETE, "/v1/x", Some(""))),
            "Incoming request: [DELETE] /v1/x"
        );
        // `OPTIONS *` (the asterisk target) has no path either, and no route to match it with.
        assert_eq!(
            request_log_line(&request(Method::OPTIONS, "*", Some("127.0.0.1:8000"))),
            "Incoming request: [OPTIONS] *"
        );
    }

    /// A verb this crate's router never names - the extension method - is still printable: the
    /// line is about what arrived, and nothing here assumes a fixed verb set.
    #[test]
    fn any_method_this_server_sees_is_printable() {
        assert_eq!(
            request_log_line(&request(
                Method::from_bytes(b"PURGE").expect("an ASCII extension method"),
                "/v1/services/pets",
                Some("127.0.0.1:8000"),
            )),
            "Incoming request: [PURGE] http://127.0.0.1:8000/v1/services/pets"
        );
        assert_eq!(
            request_log_line(&request(
                Method::OPTIONS,
                "/v1/services/pets",
                Some("127.0.0.1:8000"),
            )),
            "Incoming request: [OPTIONS] http://127.0.0.1:8000/v1/services/pets"
        );
    }

    /// A non-UTF-8 `Host` is the one header shape that cannot be printed, and it must degrade to
    /// the target rather than panic in a `panic = "abort"` build.
    #[test]
    fn a_non_utf8_host_falls_back_to_the_target() {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri("/v1/services/health");
        // `HeaderValue` accepts arbitrary bytes; `to_str` is the only place they can fail.
        builder = builder.header(
            header::HOST,
            HeaderValue::from_bytes(b"\xff\xfe").expect("a header value of arbitrary bytes"),
        );
        let request = builder.body(Body::empty()).expect("a request");
        assert_eq!(
            request_log_line(&request),
            "Incoming request: [GET] /v1/services/health"
        );
    }
}
