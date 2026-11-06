//! The `/v1/harness` mount, proven THROUGH the services gateway (T6).
//!
//! Every request here goes over a real TCP socket to a real `axum::serve` of
//! `typecrate_services::server::app(state)` - the exact router `src/bin/server.rs` serves - so
//! what is under test is the BRIDGE, not a re-run of the harness's own suite:
//!
//!   * the discovered route module (`packages/typecrate/harness/endpoint/harness/harness-route.rs`)
//!     is mounted on the gateway's `/v1/harness` prefix and hands the request to
//!     `typecrate_harness::api` with `AppState::database_root` as the harness runtime root, so the
//!     service document, `health`, the paged collections, the tool catalogue and the config read all
//!     answer exactly as they do on the standalone `typecrate-harness-server`;
//!   * matchit resolves the STATIC `/v1/harness/stream` ahead of the `/v1/harness/{*rest}`
//!     catch-all - the two handlers answer a wrong verb with two different sentences, so a single
//!     status code cannot hide a precedence regression (see
//!     `the_static_stream_route_is_the_one_the_router_picks`);
//!   * the `harness.v1` WebSocket upgrade completes THROUGH the new gateway layer (`from_fn`
//!     middleware plus a `DefaultBodyLimit`), the `welcome` frame arrives, and a `subscribe` on a
//!     session created over REST is answered with `subscription_state` quoting the SAME epoch the
//!     REST responses quote;
//!   * the gateway's own additions behave: a CORS preflight is answered with a header-only `204`
//!     before routing (the `@underload/service` policy, including
//!     `Access-Control-Allow-Private-Network: true`), while a bare `OPTIONS` is still routed, and
//!     the 1 MiB `DefaultBodyLimit` governs the extractor endpoints WITHOUT clipping the harness's
//!     own 1 MiB / 16 MiB body caps (axum applies that layer to extractors, not to a handler that
//!     reads `Request` itself - `axum-core-0.5.6/src/extract/default_body_limit.rs`).
//!
//! One fixture per test, under the repository-root `Temp/typecrate-harness/gateway/<pid>-<test>/`
//! (AGENTS.md: scratch lives in `Temp/`, never the OS temp), removed in `Drop`. The root is unique
//! per test on purpose: the harness caches one runtime per database root for the life of the
//! process (`packages/typecrate/harness/src/runtime.rs`), so two tests sharing a directory would
//! silently share sessions, revisions and the epoch, and an assertion about a FRESH store (`limit:
//! 0`, `revision: 0`) would depend on test order. The `<pid>` prefix additionally separates
//! concurrent `cargo test` processes.
//!
//! Compiled only for non-wasm targets, like every other HTTP test in this package: the harness
//! dependency and the server module are both behind the inverse-wasm gate in `Cargo.toml`.
#![cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::ACCESS_CONTROL_ALLOW_HEADERS;
use reqwest::header::ACCESS_CONTROL_ALLOW_METHODS;
use reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use typecrate_services::server::MAX_REQUEST_BODY_BYTES;
use typecrate_services::server::{AppState, app, normalize_lexically};

/// How long a frame or a shutdown may take before this test calls it hung. Everything here is
/// in-process and local: sub-millisecond is the real answer, so 5s only catches a hang.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// `contract::HARNESS_EPOCH_HEADER`, restated as a literal: this test is the CLIENT's view, and a
/// client is written against the wire name, not against a Rust constant it also owns.
const EPOCH_HEADER: HeaderName = HeaderName::from_static("x-harness-epoch");

/// The subprotocol both halves of the handshake name. Offered by the client here and echoed by the
/// mount (`stream::upgrade` -> `upgrade.protocols([harness.v1])`); a client that gets no echo has
/// not been accepted into `harness.v1`, whatever the status line says.
const SUBPROTOCOL_HEADER: HeaderName = HeaderName::from_static("sec-websocket-protocol");

/// The header that makes an `OPTIONS` a CORS preflight: `src/server/mod.rs::is_cors_preflight`
/// refuses to short-circuit without it, and this file asserts both directions.
const REQUEST_METHOD_HEADER: HeaderName = HeaderName::from_static("access-control-request-method");

/// Chrome's Private Network Access request header (the pair of the allow header below).
const REQUEST_PRIVATE_NETWORK_HEADER: HeaderName =
    HeaderName::from_static("access-control-request-private-network");

/// The PNA header the gateway publishes (`src/server/mod.rs`), the one Chrome requires on a
/// private-network preflight and the reason the harness playground can talk to this host at all.
const PRIVATE_NETWORK_ALLOW_HEADER: HeaderName =
    HeaderName::from_static("access-control-allow-private-network");

/// `contract::SERVICE_NAME` - the published service name, which is deliberately NOT the crate name.
const SERVICE_NAME: &str = "noobscript-harness";

/// `contract::MAX_BODY_BYTES`: the cap every harness route carries except the submit route. Below
/// `MAX_REQUEST_BODY_BYTES` on purpose, which is what makes the two 413s distinguishable.
const HARNESS_BODY_CAP: usize = 1_000_000;

/// `contract::MAX_SUBMIT_BODY_BYTES`, the cap of `POST /v1/harness/sessions/{id}/tasks` and the
/// reason the gateway's 1 MiB layer must not apply to a handler that reads `Request` itself.
const HARNESS_SUBMIT_CAP: usize = 16 * 1024 * 1024;

/// The stream mount's own 405 sentence (`src/stream/mod.rs`, `gate::METHOD`). The REST dispatcher
/// would answer the same request with `Use GET for /v1/harness/stream.`, so this literal is what
/// proves the ROUTER picked the stream handler - i.e. the static route beat the catch-all.
const STREAM_METHOD_SENTENCE: &str = "The stream upgrade is a GET.";

/// The harness's non-upgrade 400 (`api.ts:3911`, kept verbatim in `src/stream/mod.rs`).
const STREAM_UPGRADE_SENTENCE: &str = "GET /stream expects a WebSocket upgrade (Sec-WebSocket-Key/Version) for the harness.v1 subprotocol.";

/// The three rows the route file declares, verbatim, in registration order: the scan-root-relative
/// path `packages/typecrate/harness/endpoint/harness/harness-route.rs` sorts before every
/// `packages/typecrate/services/examples/...` route file, so these three are the FIRST rows of
/// `GET /v1/services/routes`. Pinned here as (method, path, summary) - `tests/server_http.rs` pins
/// the same triples inside the whole table.
const HARNESS_ROWS: [(&str, &str, &str); 3] = [
    ("ANY", "/v1/harness", "Underload harness service document"),
    (
        "ANY",
        "/v1/harness/stream",
        "Underload harness realtime stream (harness.v1 WebSocket)",
    ),
    (
        "ANY",
        "/v1/harness/{*rest}",
        "Underload harness API resources",
    ),
];

/// A private, writable data root for one test - the harness runtime creates its own folders below
/// it on the first request, exactly as it does on the standalone listener.
struct HarnessRoot {
    root: PathBuf,
}

impl HarnessRoot {
    /// `<repo>/Temp/typecrate-harness/gateway/<pid>-<test>`, freshly created.
    fn new(test_name: &str) -> Self {
        // <repo>/packages/typecrate/services -> ../../../ == repository root, then the scratch
        // tree this stage owns (AGENTS.md).
        let root = normalize_lexically(&Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../../Temp/typecrate-harness/gateway/{}-{test_name}",
            std::process::id()
        )));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)
            .unwrap_or_else(|err| panic!("could not create `{}`: {err}", root.display()));

        Self { root }
    }

    /// The state handed to `app()`: the harness bridge reads exactly this field and passes it to
    /// `api::handle_request` as the runtime root.
    fn state(&self) -> AppState {
        AppState::new(self.root.clone())
    }

    /// Every folder directly below the data root, sorted: the proof of WHICH root the runtime
    /// mounted through the gateway, since the harness creates its own folder on the first write.
    fn path(&self) -> &Path {
        &self.root
    }

    /// Every folder directly below the data root, sorted: the proof that the gateway mount
    /// really created the store rather than answering from a different root.
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.root)
            .unwrap_or_else(|err| panic!("could not read `{}`: {err}", self.root.display()))
            .map(|entry| {
                entry
                    .expect("the data root yields readable entries")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }
}

impl Drop for HarnessRoot {
    fn drop(&mut self) {
        // Best effort: disposable scratch state under the repository-root `Temp/`.
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The library router on an ephemeral loopback port: the same `app()` the binary serves.
struct TestServer {
    base_url: String,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(state: AppState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral loopback port is available");
        let local = listener
            .local_addr()
            .expect("the bound listener reports its address");

        let (sender, receiver) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app(state))
                .with_graceful_shutdown(async move {
                    let _ = receiver.await;
                })
                .await
                .expect("serve returns Ok once it has shut down gracefully");
        });

        Self {
            base_url: format!("http://{local}"),
            shutdown: sender,
            handle,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// The same address as a `ws://` target, for the `harness.v1` handshake. The authority is the
    /// only part that changes scheme: the path is the router's own, so the upgrade is aimed at the
    /// exact mount the REST calls use.
    fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.base_url.trim_start_matches("http://"))
    }

    async fn shutdown(self) {
        let Self {
            shutdown: sender,
            handle,
            ..
        } = self;
        let _ = sender.send(());
        tokio::time::timeout(READ_TIMEOUT, handle)
            .await
            .expect("graceful shutdown finishes promptly")
            .expect("the serve task does not panic");
    }
}

/// No pooled connections: each request is its own socket, so a keep-alive can never be mistaken
/// for a live server, and a large upload cannot be reused across assertions.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::ZERO)
        .build()
        .expect("a default client with no TLS backend builds for http://")
}

/// One HTTP answer, fully read: status, the exact header map, the raw body text, and that text
/// parsed as JSON when it IS JSON.
///
/// The parse is deliberately lenient (`Null` for anything that is not JSON), because two of the
/// answers under test are not JSON by design: the gateway's `204` preflight has no body at all, and
/// axum's own body-limit rejection is a `text/plain` sentence. Those two are pinned by their exact
/// BYTES below, and every harness response is additionally pinned through `body`, so a body that
/// stopped being the contract's JSON still fails an assertion - it is just a clearer failure when the
/// raw text is what changed.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: JsonValue,
    text: String,
}

impl Reply {
    async fn read(response: reqwest::Response) -> Self {
        let status = response.status();
        let headers = response.headers().clone();
        let text = response.text().await.expect("the body is readable");
        let body = serde_json::from_str(&text).unwrap_or(JsonValue::Null);

        Self {
            status,
            headers,
            body,
            text,
        }
    }

    async fn send(request: reqwest::RequestBuilder) -> Self {
        Self::read(
            request
                .send()
                .await
                .unwrap_or_else(|err| panic!("the request completes: {err}")),
        )
        .await
    }

    fn header(&self, name: &HeaderName) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    /// The epoch this answer quotes; every harness JSON response carries it.
    fn epoch(&self) -> String {
        self.header(&EPOCH_HEADER)
            .expect("X-Harness-Epoch on every harness response")
    }

    /// `/error/message`, which is the single error envelope the contract publishes.
    fn message(&self) -> String {
        self.body
            .pointer("/error/message")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned()
    }
}

/// `GET /v1/harness` and its epoch - the value the stream's 101 and every frame must quote back.
async fn boot(server: &TestServer) -> (Reply, String) {
    let reply = Reply::send(client().get(server.url("/v1/harness"))).await;
    // Read the epoch BEFORE the tuple moves `reply`; every later assertion in these tests quotes
    // this one value, so REST, the 101 and the frames are compared against a single truth.
    let epoch = reply.epoch();

    (reply, epoch)
}

/// `POST /v1/harness/sessions` with the minimum body the contract accepts, and the id it created.
///
/// No `settings` block on purpose: the deployment defaults (`GET /v1/harness/config`) are what make
/// a bare create legal, and passing credentials here would hide a gateway bug behind a mock.
async fn create_session(server: &TestServer) -> String {
    let reply = Reply::send(
        client()
            .post(server.url("/v1/harness/sessions"))
            .json(&json!({ "harness": "instruct" })),
    )
    .await;

    assert_eq!(
        reply.status,
        StatusCode::CREATED,
        "a bare `instruct` session creates through the gateway: {}",
        reply.text
    );

    reply
        .body
        .pointer("/session/id")
        .and_then(JsonValue::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| panic!("the created session names its id: {}", reply.text))
        .to_owned()
}

/// One WS client: the socket plus the handshake response, and the frame reader that skips the
/// heartbeats the protocol may interleave at any point.
struct Stream {
    socket: WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    status: StatusCode,
    headers: HeaderMap,
}

impl Stream {
    /// Completes the `harness.v1` handshake against the gateway router.
    async fn connect(server: &TestServer, subprotocol: Option<&str>) -> Self {
        let mut request = server
            .ws_url("/v1/harness/stream")
            .into_client_request()
            .expect("the stream URL is a valid client request");

        match subprotocol {
            Some(protocol) => {
                request.headers_mut().insert(
                    SUBPROTOCOL_HEADER,
                    protocol
                        .parse()
                        .unwrap_or_else(|err| panic!("an ASCII subprotocol: {err}")),
                );
            }
            None => {
                request.headers_mut().remove(SUBPROTOCOL_HEADER);
            }
        }

        let (socket, response) = tokio::time::timeout(READ_TIMEOUT, connect_async(request))
            .await
            .expect("the handshake completes")
            .unwrap_or_else(|err| panic!("the upgrade is accepted: {err:?}"));

        Self {
            socket,
            status: response.status(),
            headers: response.headers().clone(),
        }
    }

    async fn send(&mut self, frame: JsonValue) {
        let text = serde_json::to_string(&frame).expect("a frame serialises");

        tokio::time::timeout(READ_TIMEOUT, self.socket.send(WsMessage::Text(text.into())))
            .await
            .expect("the frame is written")
            .expect("the socket accepts the frame");
    }

    /// The next frame whose `type` is `expected`; only a `heartbeat` may take its slot.
    async fn expect_frame(&mut self, expected: &str) -> JsonValue {
        loop {
            let message = tokio::time::timeout(READ_TIMEOUT, self.socket.next())
                .await
                .expect("a frame arrives")
                .expect("the stream yields a message")
                .expect("the socket is readable");

            match message {
                WsMessage::Text(text) => {
                    let frame: JsonValue = serde_json::from_str(text.as_str())
                        .unwrap_or_else(|err| panic!("a JSON text frame, got `{text}`: {err}"));

                    if frame["type"] == json!(expected) {
                        return frame;
                    }

                    assert_eq!(
                        frame["type"],
                        json!("heartbeat"),
                        "only a `heartbeat` may interleave a `{expected}` frame, got: {frame}"
                    );
                }
                other => panic!("an unexpected websocket frame: {other:?}"),
            }
        }
    }
}

/// The four gateway headers, asserted as exact values on ANY response, because the layer is the
/// transport's promise and no route is allowed to answer without it.
fn assert_gateway_headers(headers: &HeaderMap) {
    for name in [
        ACCESS_CONTROL_ALLOW_ORIGIN,
        ACCESS_CONTROL_ALLOW_METHODS,
        ACCESS_CONTROL_ALLOW_HEADERS,
    ] {
        assert_eq!(
            headers.get(&name).and_then(|value| value.to_str().ok()),
            Some("*"),
            "{name} must be the wildcard the gateway publishes"
        );
    }
    assert_eq!(
        headers
            .get(&PRIVATE_NETWORK_ALLOW_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("true"),
        "a response without the private-network allowance breaks every LAN client"
    );
}

/// The five members of one harness page body, pinned exactly except for `observed_at` (the server
/// clock) - which is why `revision`/`limit`/`next_cursor`/`items` are still exact here.
fn assert_page(reply: &Reply, items: usize, revision: u64) {
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "a page is a 200: {}",
        reply.text
    );
    assert_eq!(reply.body["items"].as_array().map(Vec::len), Some(items));
    assert_eq!(
        reply.body["limit"],
        json!(items),
        "`limit` echoes the EFFECTIVE page size: {}",
        reply.text
    );
    assert_eq!(reply.body["next_cursor"], JsonValue::Null);
    assert_eq!(reply.body["revision"], json!(revision));
    assert!(reply.body["observed_at"].is_u64(), "a millisecond stamp");
}

// ---------------------------------------------------------------------------
// 1 - the service document
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_service_document_answers_on_the_gateway_router() {
    let root = HarnessRoot::new("service-document");
    let server = TestServer::start(root.state()).await;

    let (reply, epoch) = boot(&server).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    // The harness owns this content type (it writes the response itself, not axum's `Json`), so
    // the charset is part of the answer the gateway forwards unchanged.
    assert_eq!(
        reply
            .header(&HeaderName::from_static("content-type"))
            .as_deref(),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(reply.body["service"], json!(SERVICE_NAME));
    assert_eq!(reply.body["api_version"], json!("v1"));
    // The mount the gateway actually answers on, echoed back as the contract's `api_root`: a
    // client boots from this value, so a router that mounted the bridge elsewhere would show here.
    assert_eq!(reply.body["api_root"], json!("/v1/harness"));
    assert_eq!(reply.body["epoch"], json!(epoch));
    assert_eq!(
        reply.body["links"]["stream"],
        json!("/v1/harness/stream"),
        "the advertised stream link is the path this router really serves"
    );
    assert_eq!(
        reply.body["capabilities"]["websocket_stream"],
        json!(true),
        "the stream capability is what the 101 below depends on"
    );
    assert_gateway_headers(&reply.headers);

    // The root is a BOUNDARY, not a document root: a pure read creates nothing below it (the same
    // policy `src/server/mod.rs` states for `AppState::database_root`, which the harness honours -
    // the folder appears with the first WRITE, asserted in the create test below). A read-only boot
    // therefore cannot be mistaken for one that inherited the standalone server's layout.
    assert_eq!(
        root.entries(),
        Vec::<String>::new(),
        "a read-only boot leaves the configured data root untouched"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2 - the reads and the one write
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_the_empty_page_a_created_session_and_the_catalogue_reads_answer_through_the_gateway()
 {
    let root = HarnessRoot::new("reads-and-write");
    let server = TestServer::start(root.state()).await;
    let http = client();

    let (_, epoch) = boot(&server).await;

    // `health` is the smallest whole-body assertion available: three keys, a fresh store, and the
    // same epoch the boot document quoted.
    let health = Reply::send(http.get(server.url("/v1/harness/health"))).await;
    assert_eq!(health.status, StatusCode::OK, "{}", health.text);
    assert_eq!(
        health.body,
        json!({"ok": true, "sessions": 0, "epoch": epoch})
    );

    // A fresh collection is an empty page: `limit` is the EFFECTIVE size (0), the cursor is null
    // and the collection revision has never been bumped.
    let empty = Reply::send(http.get(server.url("/v1/harness/sessions"))).await;
    assert_page(&empty, 0, 0);

    // The write: the shipped `instruct` harness plus nothing else, which the deployment defaults
    // (base_url/model) are enough to create.
    let id = create_session(&server).await;

    let created = Reply::send(http.get(server.url(&format!("/v1/harness/sessions/{id}")))).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.text);
    assert_eq!(created.body["session"]["id"], json!(id));
    assert_eq!(created.body["session"]["harness"], json!("instruct"));

    // And the page lists it: one item, effective size 1, and the collection counter at 1 because
    // exactly one write has landed.
    let listed = Reply::send(http.get(server.url("/v1/harness/sessions"))).await;
    assert_page(&listed, 1, 1);
    assert_eq!(listed.body["items"][0]["id"], json!(id));

    // The write is also what makes the harness create its own folder BELOW the configured root:
    // `<root>/harness/state.json` (`persistence::HARNESS_DATABASE_DIR`), i.e. the data landed on
    // the path the gateway's `AppState` carries, not on the harness package's own root.
    assert_eq!(
        root.entries(),
        vec!["harness".to_owned()],
        "the first writer creates its folder below the root, and nothing else"
    );
    assert!(
        root.path().join("harness").join("state.json").is_file(),
        "the durable document is below the root the state carries"
    );

    // The tool catalogue: the shipped registry's 35 rows (`tools/catalogue.rs`), and the
    // contract's own `timeNow` among them. The DEFAULT page is 25 of those 35
    // (`contract::PAGE_LIMIT_DEFAULT`), which is why this read asks for the maximum page - a
    // gateway bug that dropped the query string would show up here as 25 rows and a cursor.
    let tools = Reply::send(http.get(server.url("/v1/harness/tools?limit=100"))).await;
    assert_eq!(tools.status, StatusCode::OK, "{}", tools.text);
    let names: Vec<&str> = tools.body["items"]
        .as_array()
        .expect("a tool page carries an items array")
        .iter()
        .map(|row| row["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        names.len(),
        35,
        "the whole shipped catalogue is reachable through the gateway"
    );
    assert_eq!(
        tools.body["limit"],
        json!(35),
        "`limit` echoes the EFFECTIVE size"
    );
    assert_eq!(
        tools.body["next_cursor"],
        JsonValue::Null,
        "the maximum page holds the whole registry"
    );
    assert!(
        names.contains(&"timeNow"),
        "`timeNow` is a shipped tool and must be reachable through the gateway: {names:?}"
    );

    // The config read: the key-free defaults document, on its untouched revision 0.
    let config = Reply::send(http.get(server.url("/v1/harness/config"))).await;
    assert_eq!(config.status, StatusCode::OK, "{}", config.text);
    assert_eq!(
        config.body["revision"],
        json!(0),
        "a read never bumps the config counter"
    );
    assert!(
        config.body["defaults"].is_object(),
        "the defaults block is what makes the bare create legal: {}",
        config.text
    );

    // A path the mount does not own is still the DISPATCHER's 404, naming the canonical root -
    // i.e. the catch-all hands over, it does not swallow.
    let missing = Reply::send(http.get(server.url("/v1/harness/nothing-here"))).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.text);
    assert_eq!(
        missing.message(),
        "Unknown API route: GET /v1/harness/nothing-here"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3 - the stream mount
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_harness_v1_upgrade_completes_through_the_gateway_layer() {
    let root = HarnessRoot::new("stream-handshake");
    let server = TestServer::start(root.state()).await;
    let (_, epoch) = boot(&server).await;
    let session_id = create_session(&server).await;

    let mut stream = Stream::connect(&server, Some("harness.v1")).await;

    // The 101 survives the `from_fn` layer: the middleware only touches response headers, and the
    // handover travels in the REQUEST's extensions.
    assert_eq!(stream.status, StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(
        stream
            .headers
            .get(SUBPROTOCOL_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("harness.v1"),
        "the offered subprotocol must be echoed, or the client's socket is not `harness.v1`"
    );
    assert_eq!(
        stream
            .headers
            .get(&EPOCH_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        Some(epoch.clone()),
        "the 101 quotes the SAME epoch as the REST responses on this root"
    );
    assert_gateway_headers(&stream.headers);

    // First frame is always `welcome`, and it names the protocol and the epoch.
    let welcome = stream.expect_frame("welcome").await;
    assert_eq!(welcome["type"], json!("welcome"));
    assert_eq!(welcome["protocol"], json!("harness.v1"));
    assert_eq!(welcome["epoch"], json!(epoch));
    assert!(
        welcome["limits"].is_object(),
        "the limits block is contract data"
    );

    // A session scope, subscribed through the gateway, reconciles against the same epoch.
    stream
        .send(json!({
            "type": "subscribe",
            "subscription_id": "gateway-1",
            "scope": "session",
            "session_id": session_id,
        }))
        .await;

    let state = stream.expect_frame("subscription_state").await;
    assert_eq!(state["type"], json!("subscription_state"));
    assert_eq!(state["subscription_id"], json!("gateway-1"));
    assert_eq!(state["scope"], json!("session"));
    assert_eq!(state["epoch"], json!(epoch));
    let publishes: Vec<&str> = state["publishes"]
        .as_array()
        .expect("a subscription states what it publishes")
        .iter()
        .map(|kind| kind.as_str().unwrap_or_default())
        .collect();
    assert!(
        publishes.contains(&"session.updated"),
        "the `session` scope publishes session updates: {publishes:?}"
    );
    assert!(
        state["revisions"].is_array(),
        "the baseline revisions are the reconciliation anchor: {state}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_plain_get_on_the_stream_mount_is_the_contracts_400() {
    let root = HarnessRoot::new("stream-plain-get");
    let server = TestServer::start(root.state()).await;

    let reply = Reply::send(client().get(server.url("/v1/harness/stream"))).await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(reply.message(), STREAM_UPGRADE_SENTENCE);
    assert_gateway_headers(&reply.headers);

    server.shutdown().await;
}

/// The precedence proof, and the reason it has to be a DIFFERENT SENTENCE rather than a status
/// code: both handlers answer a wrong verb with a 405, but only the stream mount says
/// `The stream upgrade is a GET.`. If matchit ever preferred `/v1/harness/{*rest}` for
/// `/v1/harness/stream`, the REST dispatcher's `Use GET for /v1/harness/stream.` would land here
/// instead - and a WebSocket client would then be talking to the JSON dispatcher.
#[tokio::test]
async fn the_static_stream_route_is_the_one_the_router_picks() {
    let root = HarnessRoot::new("stream-precedence");
    let server = TestServer::start(root.state()).await;

    let reply = Reply::send(client().delete(server.url("/v1/harness/stream"))).await;

    assert_eq!(
        reply.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "{}",
        reply.text
    );
    assert_eq!(reply.message(), STREAM_METHOD_SENTENCE);

    // The same verb two segments down belongs to the CATCH-ALL, and it answers with the REST
    // dispatcher's own sentence: the two mounts are genuinely separate in the table.
    let nested = Reply::send(client().delete(server.url("/v1/harness/stream/anything"))).await;
    assert_eq!(
        nested.status,
        StatusCode::NOT_FOUND,
        "a path below the stream is not a route: {}",
        nested.text
    );
    assert_eq!(
        nested.message(),
        "Unknown API route: DELETE /v1/harness/stream/anything"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4 - the listing and the gateway's own behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_listing_reports_the_three_harness_rows_first_and_as_any() {
    let root = HarnessRoot::new("listing-rows");
    let server = TestServer::start(root.state()).await;

    let reply = Reply::send(client().get(server.url("/v1/services/routes"))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);

    // (method, path, summary) per row, in the listing's own order.
    let rows: Vec<(&str, &str, &str)> = reply
        .body
        .as_array()
        .expect("the listing is an array")
        .iter()
        .map(|row| {
            (
                row["method"].as_str().unwrap_or_default(),
                row["path"].as_str().unwrap_or_default(),
                row["summary"].as_str().unwrap_or_default(),
            )
        })
        .collect();

    // The harness rows come FIRST because that is the scanner's byte-wise order of the route-file
    // paths (`packages/typecrate/harness/...` < `packages/typecrate/services/...`).
    assert_eq!(&rows[..HARNESS_ROWS.len()], &HARNESS_ROWS[..]);
    assert_eq!(
        rows.iter()
            .filter(|(_, path, _)| path.starts_with("/v1/harness"))
            .count(),
        3,
        "the bridge registers exactly three paths: {rows:?}"
    );
    // No builtin path is claimed by the bridge, and no bridge path is claimed twice.
    for (_, path, _) in &rows[..HARNESS_ROWS.len()] {
        assert!(
            ![
                "/",
                "/playground.css",
                "/playground.js",
                "/v1/services/routes"
            ]
            .contains(path),
            "the harness bridge shadows the builtin {path}: {rows:?}"
        );
    }

    // The socket agrees with the listing on the one thing only the gateway decides: `ANY` means
    // every verb reaches the handler. A verb no harness route declares is the DISPATCHER's 405,
    // with the allowed verbs of the route it matched.
    let teapot = Reply::send(client().request(
        reqwest::Method::from_bytes(b"TEAPOT").expect("a usable method token"),
        server.url("/v1/harness/health"),
    ))
    .await;
    assert_eq!(
        teapot.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "an undeclared verb reaches the dispatcher through an ANY mount: {}",
        teapot.text
    );
    assert_eq!(teapot.message(), "Use GET for /v1/harness/health.");

    server.shutdown().await;
}

#[tokio::test]
async fn a_cors_preflight_is_answered_before_routing_and_a_bare_options_is_still_routed() {
    let root = HarnessRoot::new("preflight");
    let server = TestServer::start(root.state()).await;
    let http = client();

    // A real Chrome PNA preflight: OPTIONS + the verb it wants + the private-network ask.
    let preflight = Reply::send(
        http.request(reqwest::Method::OPTIONS, server.url("/v1/harness"))
            .header(ACCESS_CONTROL_ALLOW_ORIGIN, "http://example.invalid")
            .header(REQUEST_METHOD_HEADER, "POST")
            .header(REQUEST_PRIVATE_NETWORK_HEADER, "true"),
    )
    .await;

    assert_eq!(
        preflight.status,
        StatusCode::NO_CONTENT,
        "a preflight is answered by the transport: {}",
        preflight.text
    );
    assert_eq!(preflight.text, "", "a 204 has no body to describe");
    assert_eq!(
        preflight.header(&HeaderName::from_static("content-type")),
        None,
        "a body-less answer must not claim a media type"
    );
    assert_gateway_headers(&preflight.headers);

    // The same on a path the harness answers through the catch-all, and on a builtin path.
    for path in ["/v1/harness/health", "/v1/services/health"] {
        let nested = Reply::send(
            http.request(reqwest::Method::OPTIONS, server.url(path))
                .header(REQUEST_METHOD_HEADER, "GET"),
        )
        .await;
        assert_eq!(
            nested.status,
            StatusCode::NO_CONTENT,
            "{path} answers its preflight before routing: {}",
            nested.text
        );
        assert_gateway_headers(&nested.headers);
    }

    // A plain `OPTIONS` with no declared method is NOT a preflight: it is routed, and here the
    // harness dispatcher owns the answer (`src/server/mod.rs` documents why hono's condition is
    // the one this port keeps).
    let bare =
        Reply::send(client().request(reqwest::Method::OPTIONS, server.url("/v1/harness"))).await;
    assert_eq!(bare.status, StatusCode::METHOD_NOT_ALLOWED, "{}", bare.text);
    assert_eq!(bare.message(), "Use GET for /v1/harness.");
    assert_gateway_headers(&bare.headers);

    server.shutdown().await;
}

/// The body-limit contract, in three parts.
///
/// `MAX_REQUEST_BODY_BYTES` is a [`DefaultBodyLimit`], and that layer governs only extractors that
/// ask for it - a handler that takes `Request` and streams the body is bounded by ITS OWN caps
/// (axum-core's own docs say so). The harness route therefore keeps the contract's 1 MiB / 16 MiB
/// limits through the gateway instead of silently inheriting 1 MiB, and the two 413s below are
/// told apart by their bodies: the harness sentence is a JSON envelope, axum's rejection is not.
#[tokio::test]
async fn the_gateway_body_cap_governs_extractors_not_the_harness_route() {
    let root = HarnessRoot::new("body-limits");
    let server = TestServer::start(root.state()).await;
    let http = client();

    // Above the gateway's 1 MiB layer, far below the submit route's 16 MiB cap.
    let filler = "x".repeat(1_500_000);
    assert!(filler.len() > MAX_REQUEST_BODY_BYTES);
    assert!(filler.len() < HARNESS_SUBMIT_CAP);

    // 1. The harness accepts it: an unknown session is a DOMAIN 404, refused before the body is
    //    parsed, and never a framework 413.
    let submit = Reply::send(
        http.post(server.url("/v1/harness/sessions/does-not-exist/tasks"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(format!(r#"{{"input":"{filler}"}}"#)),
    )
    .await;
    assert_eq!(
        submit.status,
        StatusCode::NOT_FOUND,
        "a > 1 MiB submit is not clipped by the gateway layer: {}",
        submit.text
    );
    assert_eq!(submit.message(), "Unknown session \"does-not-exist\".");
    assert_gateway_headers(&submit.headers);

    // 2. The harness cap still fires, and it fires with the contract's own sentence (this body is
    //    UNDER the gateway's 1 MiB layer, so only the harness could have refused it).
    let over_harness_cap = "y".repeat(HARNESS_BODY_CAP + 500);
    let create = Reply::send(
        http.post(server.url("/v1/harness/sessions"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(format!(r#"{{"harness":"{over_harness_cap}"}}"#)),
    )
    .await;
    assert_eq!(
        create.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "the harness's 1 000 000 byte cap is the one in force: {}",
        create.text
    );
    assert_eq!(
        create.message(),
        format!("A request body cannot exceed {HARNESS_BODY_CAP} bytes.")
    );

    // 3. The preserved layer still owns the extractor endpoints: the same-size body on
    //    `POST /v1/services/echo` is axum's own rejection, whose exact text is NOT the harness
    //    envelope - different status code coincidence, different mechanism, and the bytes say so.
    let echo = Reply::send(
        client()
            .post(server.url("/v1/services/echo"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(format!(r#"{{"value":"{filler}"}}"#)),
    )
    .await;
    assert_eq!(
        echo.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "the 1 MiB `DefaultBodyLimit` still refuses an oversized JSON body: {}",
        echo.text
    );
    // axum-core-0.5.6 `src/extract/rejection.rs` (`LengthLimitError` -> 413, "Failed to buffer the
    // request body"), as `tests/server_http.rs` also pins for this route.
    assert_eq!(
        echo.text, "Failed to buffer the request body: length limit exceeded",
        "the framework refusal, not the harness dispatcher's: {}",
        echo.text
    );
    assert_eq!(
        echo.body,
        JsonValue::Null,
        "axum's body-limit rejection is a plain-text sentence, so it must not parse as JSON: {}",
        echo.text
    );

    server.shutdown().await;
}
