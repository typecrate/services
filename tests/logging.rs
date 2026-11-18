//! The per-request line, over a real socket (the parity contract an operator sees).
//!
//! `src/server/request_log.rs` builds the line and `src/logging.rs` renders it; this file is
//! where both are proven against a real `axum::serve` on a real TCP port, because the contract
//! being matched (`packages/underload/service/src/server/start.ts:801-803`) is about what a
//! human sees in a terminal while the server runs:
//!
//! ```text
//! Incoming request: [GET] http://127.0.0.1:8000/v1/services/health
//! ```
//!
//! so every test below asserts the EXACT line, including the scheme, the authority the client
//! dialed, the query, and the ABSENCE of any level prefix or timestamp.
//!
//! # How the lines are read
//!
//! The subscriber writes to stdout in the binary; a test cannot read its own process's stdout,
//! so these tests install one process-wide `LogSubscriber` over an in-memory sink (`recorder`)
//! and read the same strings the terminal would show. One recorder for the whole binary, because
//! `tracing` has one global subscriber: per-test subscribers would fight over it and over the
//! shared callsite interest cache (`src/logging.rs`'s own tests explain that hazard).
//!
//! Tests run in parallel and share the sink, so every request carries a unique `?marker=` and
//! each assertion selects lines by that marker - exactness is preserved, because what is compared
//! is still a whole line against a whole expected string, never a substring match alone.
//!
//! The bound socket is always `127.0.0.1:0`, so the expected lines interpolate the ephemeral
//! port rather than hard-coding `:8000`; the fixed-port spelling is pinned in
//! `src/server/request_log.rs` and in `--help`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tracing::level_filters::LevelFilter;
use typecrate_services::logging::{self, LogSubscriber};
use typecrate_services::server::{AppState, app_for, normalize_lexically};

/// How long a shutdown may take before the test calls it hung.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The subprotocol the harness stream negotiates, so the upgrade below is the real
/// `harness.v1` one rather than a bare `GET` with upgrade headers.
const SUBPROTOCOL: &str = "sec-websocket-protocol";

/// The process-wide sink: every line the global subscriber rendered, in order.
///
/// Installed on first use by whichever test starts first. The filter is TRACE so the transport
/// cannot hide a line: what is under test here is WHICH requests produce a line, and the level
/// knob is pinned in `src/logging.rs`.
fn recorder() -> Arc<Mutex<Vec<u8>>> {
    static SINK: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    Arc::clone(SINK.get_or_init(|| {
        let sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber =
            LogSubscriber::shared(Arc::clone(&sink), logging::Filter::at(LevelFilter::TRACE));
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other subscriber is set in this test binary");
        // Belt and braces: a callsite `NoSubscriber` had already marked `never` must be switched
        // back on before the first request is served, or its line would never reach this sink.
        tracing::callsite::rebuild_interest_cache();
        sink
    }))
}

/// The lines recorded so far that carry `marker`, in order, whole.
fn lines_with(marker: &str) -> Vec<String> {
    let bytes = recorder().lock().expect("the sink is not poisoned").clone();
    String::from_utf8(bytes)
        .expect("the subscriber writes UTF-8")
        .lines()
        .filter(|line| line.contains(marker))
        .map(str::to_owned)
        .collect()
}

/// A private, writable data root that deletes itself: the harness bridge creates its own folders
/// below it on the first request (`<repo>/Temp/typecrate-services/logging/<pid>-<test>`).
struct ScratchRoot {
    root: PathBuf,
}

impl ScratchRoot {
    fn new(test_name: &str) -> Self {
        // <repo>/packages/typecrate/services -> ../../../ == repository root, then the scratch
        // tree this package owns (root AGENTS.md).
        let root = normalize_lexically(&Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../../Temp/typecrate-services/logging/{}-{test_name}",
            std::process::id()
        )));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)
            .unwrap_or_else(|err| panic!("could not create `{}`: {err}", root.display()));
        Self { root }
    }

    fn state(&self) -> AppState {
        AppState::new(self.root.clone())
    }
}

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        // Best effort: disposable scratch state below the repository-root `Temp/`.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The router for ONE listener on an ephemeral loopback port.
///
/// `declared` is the port the route files DECLARE, i.e. which `app_for` is built: `None` is the
/// default listener and `Some(8010)` is the listener for the group that declares 8010 - the two
/// listeners whose parity is being pinned. The socket port itself is `0` so parallel tests never
/// collide.
struct TestServer {
    local: SocketAddr,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(declared: Option<u16>, state: AppState) -> Self {
        // The recorder goes up BEFORE the listener exists. `tracing` decides a callsite's
        // interest once, and a callsite first touched while `NoSubscriber` was global is cached
        // `never` for the whole process - so a request served before this line would produce no
        // line at all, and the test would be asserting on lost output rather than on behaviour.
        let _recorder = recorder();

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral loopback port is available");
        let local = listener
            .local_addr()
            .expect("the bound listener reports its address");

        let (sender, receiver) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app_for(declared, state))
                .with_graceful_shutdown(async move {
                    let _ = receiver.await;
                })
                .await
                .expect("serve returns Ok once it has shut down gracefully");
        });

        Self {
            local,
            shutdown: sender,
            handle,
        }
    }

    /// The authority the client dials - and therefore the authority the `Host` header, and so the
    /// rebuilt URL in the log line, carries.
    fn origin(&self) -> String {
        self.local.to_string()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.local)
    }

    async fn shutdown(self) {
        let Self {
            shutdown: sender,
            handle,
            ..
        } = self;
        let _ = sender.send(());
        tokio::time::timeout(SHUTDOWN_TIMEOUT, handle)
            .await
            .expect("graceful shutdown finishes promptly")
            .expect("the serve task does not panic");
    }
}

/// No pooled connections: each request is its own socket, so a keep-alive cannot be mistaken for
/// a second request, and a test cannot be served from a connection opened by another one.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::ZERO)
        .build()
        .expect("a default client with no TLS backend builds for http://")
}

/// The one line a listener is expected to print for `path`: the TypeScript gateway's format,
/// with this server's own authority in front of the target.
fn expected_line(method: &str, origin: &str, path: &str) -> String {
    format!("Incoming request: [{method}] http://{origin}{path}")
}

#[tokio::test]
async fn a_matched_request_prints_the_gateway_line_on_the_default_listener() {
    let root = ScratchRoot::new("default-listener");
    let server = TestServer::start(None, root.state()).await;
    let path = "/v1/services/health?marker=default-listener";

    let status = client()
        .get(server.url(path))
        .send()
        .await
        .expect("the default listener answers")
        .status()
        .as_u16();
    assert_eq!(status, 200, "the health route answered");

    assert_eq!(
        lines_with("marker=default-listener"),
        vec![expected_line("GET", &server.origin(), path)],
        "one matched GET is exactly one line, spelled like the TypeScript gateway's"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_declared_listener_prints_the_same_line_with_its_own_authority() {
    // The same API path, served by the group that DECLARES 8010: the line names the listener the
    // request arrived on. This is the declared-port half of the parity contract, and the half
    // that used to print nothing at all.
    let root = ScratchRoot::new("declared-listener");
    let server = TestServer::start(Some(8010), root.state()).await;
    let path = "/v1/services/health?marker=declared-listener";

    let status = client()
        .get(server.url(path))
        .send()
        .await
        .expect("the declared listener answers")
        .status()
        .as_u16();
    assert_eq!(
        status, 200,
        "the ported health route answered on this listener"
    );

    assert_eq!(
        lines_with("marker=declared-listener"),
        vec![expected_line("GET", &server.origin(), path)],
        "the declared listener logs with the authority it was dialed with, not the default one"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_cors_preflight_prints_no_line() {
    let root = ScratchRoot::new("preflight");
    let server = TestServer::start(None, root.state()).await;
    let path = "/v1/services/health?marker=preflight";

    let response = client()
        .request(reqwest::Method::OPTIONS, server.url(path))
        .header("access-control-request-method", "GET")
        .header("origin", "http://localhost:5173")
        .send()
        .await
        .expect("the gateway answers the preflight");
    assert_eq!(
        response.status().as_u16(),
        204,
        "the preflight is answered before routing"
    );

    assert_eq!(
        lines_with("marker=preflight"),
        Vec::<String>::new(),
        "a preflight never reached a route, so parity says no line: the TypeScript's log \
         statement lives inside the routed handler, which a preflight never enters"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn an_unmatched_path_prints_no_line() {
    let root = ScratchRoot::new("unmatched");
    let server = TestServer::start(None, root.state()).await;
    let path = "/v1/services/never-mounted?marker=unmatched";

    let status = client()
        .get(server.url(path))
        .send()
        .await
        .expect("the listener answers")
        .status()
        .as_u16();
    assert_eq!(status, 404, "the fallback answered, and no route matched");

    assert_eq!(
        lines_with("marker=unmatched"),
        Vec::<String>::new(),
        "`route_layer` leaves the fallback router unwrapped, so the 404 the ROUTER itself \
         produces prints nothing - the same decision the `@underload/service` gateway makes, \
         whose log statement is inside a matched handler"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_wrong_verb_on_a_mounted_path_prints_one_line() {
    // The documented boundary: `MethodRouter::layer` wraps that path's verb fallback too, so a
    // 405 from a path this listener DOES mount is logged. The verb did arrive on a matched path,
    // and reporting the verb is what the line is for.
    let root = ScratchRoot::new("wrong-verb");
    let server = TestServer::start(None, root.state()).await;
    let path = "/v1/services/health?marker=wrong-verb";

    let status = client()
        .delete(server.url(path))
        .send()
        .await
        .expect("the listener answers")
        .status()
        .as_u16();
    assert_eq!(status, 405, "the path is mounted, the verb is not");

    assert_eq!(
        lines_with("marker=wrong-verb"),
        vec![expected_line("DELETE", &server.origin(), path)],
        "one line for the 405, carrying the verb that actually arrived"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_websocket_upgrade_prints_exactly_one_line() {
    let root = ScratchRoot::new("upgrade");
    let server = TestServer::start(None, root.state()).await;
    let path = "/v1/harness/stream?marker=upgrade";

    // The real `harness.v1` handshake: the subprotocol is what the harness stream negotiates, so
    // the 101 below proves the request went through the router as an upgrade rather than being
    // rejected before it.
    let mut request = server
        .url(path)
        .replace("http://", "ws://")
        .into_client_request()
        .expect("the stream URL is a valid client request");
    request.headers_mut().insert(
        SUBPROTOCOL,
        "harness.v1"
            .parse::<HeaderValue>()
            .expect("an ASCII subprotocol"),
    );
    let (_stream, response) = connect_async(request)
        .await
        .expect("the handshake completes");
    assert_eq!(
        response.status().as_u16(),
        101,
        "the upgrade was accepted, so the request did pass through the router"
    );

    assert_eq!(
        lines_with("marker=upgrade"),
        vec![expected_line("GET", &server.origin(), path)],
        "the upgrade is ONE request through the router, so it logs exactly once: the frames \
         that follow never re-enter it"
    );

    server.shutdown().await;
}
