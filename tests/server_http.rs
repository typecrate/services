//! Real-HTTP integration tests for the native runtime (T10).
//!
//! Everything here goes over a real TCP socket to a real `axum::serve` instance
//! built by `typecrate_services::server::app` - no stubbed transport, no in-process
//! `oneshot` fake. The point is to pin what a caller actually observes:
//!
//!   * the route table produced by the build-time generated registration is
//!     reachable through `app()` (R2), including 404/405 for what is not registered;
//!   * the shared `AppState` is carried by the router and never leaks a filesystem
//!     path into a response (R3);
//!   * method handling, JSON validation and the server-side body cap behave as
//!     documented (R3);
//!   * the two REST samples answer the documented CRUD contract over a real store file
//!     below `AppState::database_root`: `201` + `location`, `204`, the `404`/`400`/`415`/
//!     `422`/`413`/`500` bodies, the `?city=` filter, the id rule, and lazy file creation
//!     (R3);
//!   * the builtin browser surface (T18) - the embedded playground document at
//!     `GET /` and the generated endpoint listing at `GET /v1/services/routes` -
//!     serves the documented content type and security headers, lists exactly what
//!     the socket dispatches, and never leaks a source path (R2/R3/R4).
//!
//! The endpoint contract asserted below is the one `examples/endpoints/**` + `build.rs`
//! generate (owned by T11): `GET /v1/services/health`,
//! `GET /v1/services/echo/{value}?prefix=`, `POST /v1/services/echo`, plus the two REST
//! samples - six routes each over `/v1/services/locations` (the only one with a `?city=`
//! filter) and `/v1/services/pets`, on `GET`/`POST` of the collection and
//! `GET`/`PUT`/`PATCH`/`DELETE` of `{id}`. Those twelve are the crate's first endpoints that
//! WRITE, so their half of this file also pins the store file they leave below the data root.
//!
//! Compiled only for non-wasm targets: axum/tokio/reqwest are target-gated in
//! `Cargo.toml`, so the crate root cfg below makes this an empty test crate for
//! `cargo check --target wasm32-unknown-unknown --all-targets`.
#![cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header::ALLOW;
use reqwest::header::CACHE_CONTROL;
use reqwest::header::CONTENT_SECURITY_POLICY;
use reqwest::header::CONTENT_TYPE;
use reqwest::header::HeaderName;
use reqwest::header::LOCATION;
use reqwest::header::REFERRER_POLICY;
use reqwest::header::X_CONTENT_TYPE_OPTIONS;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use typecrate_services::server::{AppState, MAX_REQUEST_BODY_BYTES, app, normalize_lexically};

/// Seconds a shutdown is allowed to take before the test calls it hung. Graceful
/// shutdown here is "stop accepting, finish what is in flight", which is
/// sub-millisecond for these handlers; 5s only catches a real hang.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The `GET /v1/services/health` body, fixed by the endpoint contract. The
/// npm/crate version is part of it, so a version bump is an explicit edit here
/// (and `src/lib.rs::version_matches_the_npm_line` pins the same number).
///
/// A function rather than a `const`: `serde_json::Value` owns heap data and has a
/// `Drop` impl, which a `const` cannot hold.
fn health_body() -> JsonValue {
    json!({ "service": "typecrate-services", "version": "1.0.2" })
}

/// A scratch data root that does NOT have to exist and is never written to: the tests that
/// only call `health`/`echo`/the builtins pass one of these, which also proves a missing
/// root is not a startup error (the server only stores the path in `AppState`). It lives
/// under the repo-root `Temp/` per AGENTS.md so a test run cannot touch the shared
/// `temporary/dump` boundary or the OS temp directory.
///
/// A test that reaches the REST samples must use [`StoreFixture`] instead: those endpoints
/// really do write below the root, so they need a directory of their own that starts empty.
fn scratch_root(name: &str) -> PathBuf {
    // <repo>/packages/typecrate/services -> ../../../ == repo root, then Temp/<agent>.
    normalize_lexically(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../../Temp/opencode/server-http/{name}")),
    )
}

/// A data root for the endpoints that WRITE: freshly created, uniquely named
/// (`<pid>-<test name>`, the `tests/discovery.rs` pattern), and removed in `Drop`.
///
/// Why this exists at all: the store lock in both REST route files is a process-wide
/// `static Mutex`, so concurrent test threads share it and can interleave freely - what has
/// to be per-test is the DIRECTORY. The pid prefix additionally separates concurrent
/// `cargo test` processes, and clearing a stale directory keeps an aborted earlier run from
/// seeding ids this run would otherwise inherit.
struct StoreFixture {
    root: PathBuf,
}

impl StoreFixture {
    /// The root itself IS created eagerly (it is the configured boundary, and `entries()`
    /// below has to be able to read it); the resource folder inside it is what the endpoints
    /// are supposed to create lazily.
    fn new(test_name: &str) -> Self {
        let root = normalize_lexically(&Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../../Temp/opencode/server-http/{}-{test_name}",
            std::process::id()
        )));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)
            .unwrap_or_else(|err| panic!("could not create `{}`: {err}", root.display()));
        Self { root }
    }

    /// The state this fixture's server is built with. Cloned per call on purpose: the
    /// persistence test hands the SAME root to a second `AppState`.
    fn state(&self) -> AppState {
        AppState::new(self.root.clone())
    }

    /// `<root>/<STORE_DIR>/<STORE_FILE>` for one sample - the exact path the route file
    /// derives from `AppState::database_root`.
    fn store_path(&self, sample: &RestSample) -> PathBuf {
        self.root.join(sample.store_dir).join(sample.store_file)
    }

    /// Seeds the store with `contents`, creating the resource folder the endpoint would have
    /// created itself. Used only by the corrupt-store test.
    fn write_store(&self, sample: &RestSample, contents: &str) {
        let path = self.store_path(sample);
        fs::create_dir_all(path.parent().expect("a store path has a parent"))
            .unwrap_or_else(|err| panic!("could not create the store folder: {err}"));
        fs::write(&path, contents)
            .unwrap_or_else(|err| panic!("could not write `{}`: {err}", path.display()));
    }

    /// The store file's exact bytes, or `None` when the file is not there (which the contract
    /// says is the empty store, not an error).
    fn read_store(&self, sample: &RestSample) -> Option<String> {
        match fs::read_to_string(self.store_path(sample)) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => panic!("could not read the store file: {err}"),
        }
    }

    /// Every entry directly below the data root, sorted: the lazy-creation assertions ask
    /// "did anything at all appear here", which a `Path::exists` check cannot answer.
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

impl Drop for StoreFixture {
    fn drop(&mut self) {
        // Best effort: disposable scratch state under the repo-root `Temp/` (AGENTS.md).
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A server on an ephemeral loopback port, plus the two single-use handles needed to
/// shut it down and prove the serve task actually finished.
struct TestServer {
    base_url: String,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// Serves THE library router (`app(state)`), the same function
    /// `src/bin/server.rs` serves, so these tests cannot drift from the executable.
    async fn start(state: AppState) -> Self {
        // Port 0 == ask the OS for a free port: parallel `cargo test` workers and a
        // busy dev machine can never collide, and nothing is killed on the way in.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral loopback port is available");
        let local = listener
            .local_addr()
            .expect("the bound listener reports its address");

        // `rx` is not declared `mut`: `.await` takes the receiver by value and polls
        // it through an internal pin (tokio 1.54.1 `oneshot.rs:1268`).
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app(state))
                // The test's own graceful-shutdown path: dropping the sender would
                // also end this, but an explicit signal keeps the intent obvious.
                .with_graceful_shutdown(async move {
                    // Awaiting the receiver directly: tokio 1.54.1 removed the
                    // `Receiver::recv()` wrapper and implements `Future for
                    // Receiver<T>` with `Output = Result<(), RecvError>`
                    // (~/.cargo/.../tokio-1.54.1/src/sync/oneshot.rs:1268). The
                    // result is discarded on purpose - either a value or a dropped
                    // sender means "stop serving".
                    let _ = rx.await;
                })
                .await
                .expect("serve returns Ok once it has shut down gracefully");
        });

        Self {
            base_url: format!("http://{local}"),
            shutdown: tx,
            handle,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Signals shutdown and waits for the serve task to resolve, which is what makes
    /// the "listener is closed afterwards" assertion legal.
    async fn shutdown(self) {
        // Both halves are consumed by design: one signal, one join.
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

/// Client with the connection pool disabled, so every request is its own socket and
/// a keep-alive connection can never be mistaken for "the server is still up".
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::ZERO)
        .build()
        .expect("a default client with no TLS backend builds for http://")
}

/// Parses a body that the contract says is JSON; a parse failure shows the raw body
/// so a wrong content type is diagnosable instead of mysterious.
async fn json_body(response: reqwest::Response) -> JsonValue {
    let status = response.status();
    let text = response.text().await.expect("the body is readable");
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("status {status} body is not JSON: {text:?} ({err})"))
}

// ---------------------------------------------------------------------------
// GET: path parameters and query parameters (R3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_health_returns_the_locked_identity_json() {
    let server = TestServer::start(AppState::new(scratch_root("health"))).await;

    let response = client()
        .get(server.url("/v1/services/health"))
        .send()
        .await
        .expect("health is reachable over TCP");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    // Exact prefix; a `; charset=utf-8` suffix is not part of the contract.
    assert!(
        content_type.starts_with("application/json"),
        "expected a JSON content type, got {content_type:?}"
    );
    assert_eq!(json_body(response).await, health_body());

    server.shutdown().await;
}

#[tokio::test]
async fn get_echo_returns_its_path_segment_verbatim() {
    let server = TestServer::start(AppState::new(scratch_root("echo-path"))).await;
    let client = client();

    let plain = client
        .get(server.url("/v1/services/echo/hello"))
        .send()
        .await
        .expect("echo is reachable");
    assert_eq!(plain.status(), reqwest::StatusCode::OK);
    assert_eq!(json_body(plain).await, json!({ "value": "hello" }));

    // Multi-byte UTF-8 in a path parameter must survive routing + serialisation.
    let unicode = client
        .get(server.url("/v1/services/echo/%F0%9F%98%80"))
        .send()
        .await
        .expect("echo accepts an encoded segment");
    assert_eq!(unicode.status(), reqwest::StatusCode::OK);
    assert_eq!(json_body(unicode).await, json!({ "value": "\u{1F600}" }));

    server.shutdown().await;
}

#[tokio::test]
async fn get_echo_prefix_query_param_is_prepended() {
    let server = TestServer::start(AppState::new(scratch_root("echo-query"))).await;
    let client = client();

    let prefixed = client
        .get(server.url("/v1/services/echo/there?prefix=pre-"))
        .send()
        .await
        .expect("echo accepts a query");
    assert_eq!(prefixed.status(), reqwest::StatusCode::OK);
    assert_eq!(json_body(prefixed).await, json!({ "value": "pre-there" }));

    // An encoded query value is decoded before use, like the path case above.
    let spaced = client
        .get(server.url("/v1/services/echo/b%20c?prefix=a%20"))
        .send()
        .await
        .expect("echo accepts encoded query text");
    assert_eq!(json_body(spaced).await, json!({ "value": "a b c" }));

    // No prefix at all == the identity echo; the query parameter is optional.
    let absent = client
        .get(server.url("/v1/services/echo/plain"))
        .send()
        .await
        .expect("echo works without a query");
    assert_eq!(json_body(absent).await, json!({ "value": "plain" }));

    // An empty prefix must not inject anything (it is different from absent only in
    // that the caller asked for it explicitly).
    let empty = client
        .get(server.url("/v1/services/echo/plain?prefix="))
        .send()
        .await
        .expect("echo accepts an empty prefix");
    assert_eq!(json_body(empty).await, json!({ "value": "plain" }));

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// POST: JSON body round-trip and rejections (R3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_echo_json_body_round_trips() {
    let server = TestServer::start(AppState::new(scratch_root("echo-post"))).await;

    let response = client()
        .post(server.url("/v1/services/echo"))
        .json(&json!({ "value": "round-trip" }))
        .send()
        .await
        .expect("the echo endpoint accepts POST");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(json_body(response).await, json!({ "value": "round-trip" }));

    server.shutdown().await;
}

#[tokio::test]
async fn post_echo_rejects_malformed_json_with_400() {
    let server = TestServer::start(AppState::new(scratch_root("echo-bad-json"))).await;

    // Truncated document: a *syntax* failure, which axum always reports as 400.
    let response = client()
        .post(server.url("/v1/services/echo"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(r#"{"value": "#)
        .send()
        .await
        .expect("the request reaches the server");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    server.shutdown().await;
}

#[tokio::test]
async fn post_echo_rejects_a_value_that_is_not_a_string() {
    let server = TestServer::start(AppState::new(scratch_root("echo-bad-type"))).await;

    let response = client()
        .post(server.url("/v1/services/echo"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(r#"{"value": {"nested": true}}"#)
        .send()
        .await
        .expect("the request reaches the server");

    // The contract is `{"value": string}`, so this must be refused rather than
    // echoed. The exact code comes from axum's split between the two JSON failure
    // classes, and it is not ours to choose:
    //   * axum-0.8.9 `src/extract/rejection.rs:23`  JsonSyntaxError  -> 400
    //   * axum-0.8.9 `src/extract/rejection.rs:11`  JsonDataError    -> 422
    // serde_json routes `invalid_type` through `Error::custom` (1.0.154
    // `src/error.rs:440,435`), i.e. `Category::Data` -> 422, so 422 is the ONLY legal
    // answer for a `Json<T>` extractor: a 400 here would mean the route stopped using
    // `Json<T>`, or axum changed the mapping. `post_echo_rejects_malformed_json_with_400`
    // pins the other half of the pair (a syntax error stays 400), and the README wire
    // table documents this one as 422, so the whole expectation is now exact rather than
    // a two-value range.
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        "a non-string `value` must be rejected as a JsonDataError (422)"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn post_echo_refuses_a_body_over_the_server_cap_with_413() {
    let server = TestServer::start(AppState::new(scratch_root("echo-too-large"))).await;

    // Just over `server::MAX_REQUEST_BODY_BYTES`, which is the layer in `app()` -
    // this test pins MY limit, not a handler's behaviour.
    let oversized = format!(
        r#"{{"value": "{}"}}"#,
        "x".repeat(MAX_REQUEST_BODY_BYTES + 1024)
    );

    let response = client()
        .post(server.url("/v1/services/echo"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(oversized)
        .send()
        .await
        .expect("the request reaches the server");

    // 413 as a number: the `http` crate has renamed this status once already, and
    // the wire value is what the caller sees. Source of the code is axum-core 0.5.6
    // `src/extract/rejection.rs:41` (`LengthLimitError` = PAYLOAD_TOO_LARGE, "Failed
    // to buffer the request body"), which is what a body over the limit produces.
    assert_eq!(response.status().as_u16(), 413);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// What is NOT registered: method mismatch (405) and unknown path (404)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unsupported_method_on_a_registered_path_is_405() {
    let server = TestServer::start(AppState::new(scratch_root("methods"))).await;
    let client = client();

    // `/v1/services/health` exists (GET) but DELETE is not part of its contract.
    let deleted = client
        .delete(server.url("/v1/services/health"))
        .send()
        .await
        .expect("the request reaches the server");
    assert_eq!(deleted.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);

    // `/v1/services/echo/{value}` is GET-only; the POST body endpoint is the bare
    // `/v1/services/echo`, so POSTing to the parameterised path is not a route.
    let posted = client
        .post(server.url("/v1/services/echo/hello"))
        .json(&json!({ "value": "nope" }))
        .send()
        .await
        .expect("the request reaches the server");
    assert_eq!(posted.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);

    server.shutdown().await;
}

#[tokio::test]
async fn unknown_path_is_404() {
    let server = TestServer::start(AppState::new(scratch_root("missing"))).await;
    let client = client();

    let sibling = client
        .get(server.url("/v1/services/does-not-exist"))
        .send()
        .await
        .expect("the request reaches the server");
    assert_eq!(sibling.status(), reqwest::StatusCode::NOT_FOUND);

    let unrelated = client
        .get(server.url("/v1/nope"))
        .send()
        .await
        .expect("the request reaches the server");
    assert_eq!(unrelated.status(), reqwest::StatusCode::NOT_FOUND);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Shared state: it travels with the router, and it is never an HTTP value (R3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shared_database_root_is_carried_by_the_state_and_never_exposed_over_http() {
    let root_a = scratch_root("state-a");
    let root_b = scratch_root("state-b");
    assert_ne!(root_a, root_b);

    let server_a = TestServer::start(AppState::new(root_a.clone())).await;
    let server_b = TestServer::start(AppState::new(root_b.clone())).await;

    let response_a = client()
        .get(server_a.url("/v1/services/health"))
        .send()
        .await
        .expect("server A is reachable");
    assert_eq!(response_a.status(), reqwest::StatusCode::OK);
    let raw_a = response_a.text().await.expect("health has a body");

    let response_b = client()
        .get(server_b.url("/v1/services/echo/value?prefix=p-"))
        .send()
        .await
        .expect("server B is reachable");
    assert_eq!(response_b.status(), reqwest::StatusCode::OK);
    let raw_b = response_b.text().await.expect("echo has a body");

    // Two different data roots produce the documented bytes, i.e. the state really
    // is per-server configuration rather than ambient process state.
    //
    // The health response is compared as parsed JSON (object identity, not byte
    // order, which is not part of the contract); the echo response is compared as
    // exact text because it has a single key, so its serialisation has exactly one
    // spelling.
    let body_a: JsonValue = serde_json::from_str(&raw_a)
        .unwrap_or_else(|err| panic!("health body is not JSON: {raw_a:?} ({err})"));
    assert_eq!(body_a, health_body());
    assert_eq!(raw_b, r#"{"value":"p-value"}"#);
    for raw in [&raw_a, &raw_b] {
        for root in [&root_a, &root_b] {
            let root_text = root.display().to_string();
            assert!(
                !raw.contains(&root_text),
                "response leaked the data root: {raw}"
            );
        }
        assert!(
            !raw.contains("Temp"),
            "response looks like it leaked a path: {raw}"
        );
    }

    // The clone handed to `app()` is still the caller's own value afterwards (axum
    // clones the state into the router; it neither consumes nor mutates the
    // original), which is what lets one `AppState` back several routers.
    let state = AppState::new(root_a.clone());
    let _router = app(state.clone());
    assert_eq!(state.database_root, root_a);

    server_a.shutdown().await;
    server_b.shutdown().await;
}

#[tokio::test]
async fn graceful_shutdown_closes_the_listener() {
    let server = TestServer::start(AppState::new(scratch_root("shutdown"))).await;
    let url = server.url("/v1/services/health");

    let up = client().get(&url).send().await.expect("the server is up");
    assert_eq!(up.status(), reqwest::StatusCode::OK);
    assert_eq!(json_body(up).await, health_body());

    server.shutdown().await;

    // `shutdown()` awaited the serve task, so the listener has been dropped and the
    // port is closed again: the failure is a connect failure, not a status code.
    let error = client()
        .get(&url)
        .send()
        .await
        .expect_err("nobody is listening after a graceful shutdown");
    assert!(
        error.is_connect(),
        "expected a refused connection, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// The two REST samples: the CRUD contract one route file implements six times.
//
// `pets` and `locations` follow the same contract apart from their record fields, so the
// shared facts live in `RestSample` and each test below runs one sequence per sample. Every
// expected byte in this section was measured against a running server, and bodies are
// compared as WIRE TEXT rather than parsed JSON on purpose: a record's field order (`id`
// first, then the fields in the order the route file declares them) is part of the documented
// answer, and a parsed object cannot tell one order from another.
//
// The storage rules asserted here are the ones the route files document in their own module
// docs: one JSON array per resource at `<database-root>/<resource>/<resource>.json`; the
// folder and the file appear with the first accepted WRITE and a missing file reads as the
// empty store; every accepted change rewrites the whole array; the next id is
// `max(stored id) + 1`, so an id comes back exactly when the record holding the highest id is
// the one that was deleted; a file that exists but does not parse is a 500 and stays
// byte-identical.
// ---------------------------------------------------------------------------

/// Everything the two samples do NOT share: their paths, their store file, and the exact
/// request and answer bytes of one CRUD walk.
#[derive(Clone, Copy)]
struct RestSample {
    /// The collection path, exactly as the listing advertises it.
    collection: &'static str,
    /// `STORE_DIR` / `STORE_FILE` in the route file: the folder and file this resource owns
    /// below the data root.
    store_dir: &'static str,
    store_file: &'static str,
    /// `,"name":...}` - everything a record body holds after `{"id":<id>`, which is what lets
    /// `record`/`stored` below render any id as exact bytes.
    record_tail: &'static str,
    /// `POST` body: every mutable field, no `id`.
    post_body: &'static str,
    /// The same record with an `id` and an unknown key added: both must be ignored, because
    /// the store assigns ids.
    post_extra_body: &'static str,
    /// `PUT` body: a whole record with every field changed, plus an `id` the path must win
    /// against.
    put_body: &'static str,
    /// The `200` body of that `PUT`: the path's id, the body's fields.
    put_result: &'static str,
    /// `PATCH` body: exactly one field.
    patch_body: &'static str,
    patch_result: &'static str,
    /// `PATCH` body: two fields explicitly `null` (which must keep the stored value), one
    /// changed field, and an unknown key.
    patch_null_body: &'static str,
    patch_null_result: &'static str,
    /// `PATCH` body: an `id` (ignored - PATCH cannot renumber) next to one real change.
    patch_extra_body: &'static str,
    patch_extra_result: &'static str,
    /// The 404 body, with `{id}` standing for the id the message has to name.
    missing_template: &'static str,
    /// The one body every storage failure of this resource answers with.
    store_error_body: &'static str,
    /// The 422 body of a `POST` whose body is `[]`: it names this resource's DTO type, so it
    /// also pins that the route really extracts `Json<Input>` and not `Json<Value>`.
    reject_array_body: &'static str,
    /// Store-file bytes that exist but do not parse (two shapes: not JSON at all, and JSON
    /// whose records do not fit the type).
    corrupt_store: &'static str,
    wrong_shape_store: &'static str,
    /// The `allow` header on each path pattern - the router's own account of what is
    /// registered, which is what makes "the listed method is the registered one" positive
    /// evidence. `HEAD` is on every `GET` route because axum answers it for one.
    allow_collection: &'static str,
    allow_by_id: &'static str,
}

/// The `pets` sample, exactly as `examples/endpoints/pets/pets-core.rs` answers it.
fn pets() -> RestSample {
    RestSample {
        collection: "/v1/services/pets",
        store_dir: "pets",
        store_file: "pets.json",
        record_tail: r#","name":"Rex","species":"dog","age":3}"#,
        post_body: r#"{"name":"Rex","species":"dog","age":3}"#,
        post_extra_body: r#"{"id":99,"name":"Rex","species":"dog","age":3,"extra":true}"#,
        put_body: r#"{"id":4242,"name":"Rexy","species":"dog","age":7}"#,
        put_result: r#"{"id":1,"name":"Rexy","species":"dog","age":7}"#,
        patch_body: r#"{"age":4}"#,
        patch_result: r#"{"id":1,"name":"Rex","species":"dog","age":4}"#,
        patch_null_body: r#"{"name":null,"age":null,"species":"hound","bogus":1}"#,
        patch_null_result: r#"{"id":1,"name":"Rex","species":"hound","age":4}"#,
        patch_extra_body: r#"{"id":99,"age":6,"unknown":true}"#,
        patch_extra_result: r#"{"id":1,"name":"Rex","species":"hound","age":6}"#,
        missing_template: r#"{"error":"no pet with id {id}"}"#,
        store_error_body: r#"{"error":"pet storage failed"}"#,
        reject_array_body: "Failed to deserialize the JSON body into the target type: invalid length 0, expected struct PetInput with 3 elements at line 1 column 2",
        corrupt_store: "not json at all",
        wrong_shape_store: r#"[{"id":"one","name":"Rex","species":"dog","age":3}]"#,
        allow_collection: "GET,HEAD,POST",
        allow_by_id: "GET,HEAD,PUT,PATCH,DELETE",
    }
}

/// The `locations` sample, exactly as
/// `examples/endpoints/locations/locations-core.rs` answers it.
fn locations() -> RestSample {
    RestSample {
        collection: "/v1/services/locations",
        store_dir: "locations",
        store_file: "locations.json",
        record_tail: r#","name":"Flagship store","city":"Berlin","country":"DE"}"#,
        post_body: r#"{"name":"Flagship store","city":"Berlin","country":"DE"}"#,
        post_extra_body: r#"{"id":99,"name":"Flagship store","city":"Berlin","country":"DE","extra":true}"#,
        put_body: r#"{"id":4242,"name":"Warehouse","city":"Hamburg","country":"DE"}"#,
        put_result: r#"{"id":1,"name":"Warehouse","city":"Hamburg","country":"DE"}"#,
        patch_body: r#"{"city":"Munich"}"#,
        patch_result: r#"{"id":1,"name":"Flagship store","city":"Munich","country":"DE"}"#,
        patch_null_body: r#"{"name":null,"country":null,"city":"Antwerp","bogus":1}"#,
        patch_null_result: r#"{"id":1,"name":"Flagship store","city":"Antwerp","country":"DE"}"#,
        patch_extra_body: r#"{"id":99,"city":"Berlin","unknown":true}"#,
        patch_extra_result: r#"{"id":1,"name":"Flagship store","city":"Berlin","country":"DE"}"#,
        missing_template: r#"{"error":"no location with id {id}"}"#,
        store_error_body: r#"{"error":"location storage failed"}"#,
        reject_array_body: "Failed to deserialize the JSON body into the target type: invalid length 0, expected struct LocationInput with 3 elements at line 1 column 2",
        corrupt_store: r#"{"id":1}"#,
        wrong_shape_store: r#"[{"id":"one","name":"Depot","city":"Antwerp","country":"BE"}]"#,
        allow_collection: "GET,HEAD,POST",
        allow_by_id: "GET,HEAD,PUT,PATCH,DELETE",
    }
}

/// Both samples: every shared-contract test below runs once per element.
fn rest_samples() -> [RestSample; 2] {
    [pets(), locations()]
}

/// The byte-exact record body for one id: `{"id":<id>` plus the sample's own field tail.
fn record(sample: &RestSample, id: u64) -> String {
    format!("{{\"id\":{id}{}", sample.record_tail)
}

/// The byte-exact collection body holding exactly those records, in that order (the order the
/// store keeps them, which is ascending by id).
fn stored(sample: &RestSample, ids: &[u64]) -> String {
    let bodies: Vec<String> = ids.iter().map(|id| record(sample, *id)).collect();
    format!("[{}]", bodies.join(","))
}

/// The `{id}` path with a concrete id substituted - also the value of a `201`'s `location`
/// header, which is a path on this same origin and never a filesystem path.
fn by_id_path(sample: &RestSample, id: u64) -> String {
    format!("{}/{id}", sample.collection)
}

/// The 404 body for one id: the sample's own wording with only the digits substituted, which
/// is what pins that the handler interpolates the id it was asked for.
fn missing(sample: &RestSample, id: u64) -> String {
    sample.missing_template.replace("{id}", &id.to_string())
}

/// The id every "unknown id" assertion uses, and a second one, so the message is pinned as
/// interpolated text rather than as one lucky literal.
const UNKNOWN_ID: u64 = 7;
const OTHER_UNKNOWN_ID: u64 = 4242;

/// The body of a `Json` media-type rejection (415), whether the request names a wrong media
/// type or none at all. Measured: axum 0.8 words it like this, and
/// `scripts/check-playground.mjs` repeats the same bytes in its fixtures.
const REJECT_MEDIA_TYPE: &str = "Expected request with `Content-Type: application/json`";

/// The body of the server-level body-cap rejection (413), which the `DefaultBodyLimit` layer
/// in `app()` produces before any endpoint sees the request.
const REJECT_BODY_TOO_LARGE: &str = "Failed to buffer the request body: length limit exceeded";

/// Every rejection an extractor produces is plain text, while every 404/500 an endpoint
/// produces is JSON. Asserting the media type alongside the status is what tells the two
/// apart, i.e. what proves a response came from the handler and not from the framework.
const REJECT_TEXT_PLAIN: &str = "text/plain; charset=utf-8";

/// What one HTTP exchange owes an assertion: the status, the RAW body text, and the three
/// headers the REST contract is built on (`content-type`, `location`, `allow`).
#[derive(Debug)]
struct Answer {
    status: reqwest::StatusCode,
    content_type: Option<String>,
    body: String,
    location: Option<String>,
    allow: Option<String>,
}

impl Answer {
    async fn read(response: reqwest::Response) -> Self {
        // Headers are copied out before the body is streamed, so the borrow of `response`
        // ends here and `text()` may consume it.
        let status = response.status();
        let content_type = header_value(&response, CONTENT_TYPE).map(str::to_owned);
        let location = header_value(&response, LOCATION).map(str::to_owned);
        let allow = header_value(&response, ALLOW).map(str::to_owned);
        let body = response.text().await.expect("the body is readable");
        Self {
            status,
            content_type,
            body,
            location,
            allow,
        }
    }
}

/// One request with a JSON body (`None` sends no body at all).
async fn send(
    client: &reqwest::Client,
    server: &TestServer,
    method: reqwest::Method,
    path: &str,
    body: Option<&str>,
) -> Answer {
    let media_type = body.map(|_| "application/json");
    send_with_type(client, server, method, path, body, media_type).await
}

/// One request where the caller controls the media type exactly, because `None` (no
/// `Content-Type` header) and `Some("text/plain")` are two different 415 cases.
async fn send_with_type(
    client: &reqwest::Client,
    server: &TestServer,
    method: reqwest::Method,
    path: &str,
    body: Option<&str>,
    content_type: Option<&str>,
) -> Answer {
    // `Method` is not `Copy` and the builder consumes it, so the assertion label is rendered
    // BEFORE the move (same rule as the 405 loops below).
    let label = format!("{method} {path}");
    let mut request = client.request(method, server.url(path));
    if let Some(body) = body {
        request = request.body(body.to_owned());
    }
    if let Some(content_type) = content_type {
        request = request.header(CONTENT_TYPE, content_type);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|err| panic!("{label} never reached the server: {err}"));
    Answer::read(response).await
}

#[tokio::test]
async fn rest_post_answers_201_with_the_record_and_a_location_header() {
    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-post-{}", sample.store_dir));
        let server = TestServer::start(store.state()).await;
        let client = client();

        // An empty store is an empty array: not a 404, not a 500, not an error page.
        let empty = send(
            &client,
            &server,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(empty.status, reqwest::StatusCode::OK);
        assert_eq!(empty.content_type.as_deref(), Some("application/json"));
        assert_eq!(empty.body, "[]");

        // The created record is the answer, and the `location` header points at it on this
        // same origin (the only path-shaped value any response of this crate carries).
        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);
        assert_eq!(created.content_type.as_deref(), Some("application/json"));
        assert_eq!(created.body, record(&sample, 1));
        assert_eq!(
            created.location.as_deref(),
            Some(by_id_path(&sample, 1).as_str())
        );

        // The header is a promise the same server keeps: dialling it answers the same bytes.
        let header_path = created.location.clone().expect("a 201 names its location");
        let fetched = send(&client, &server, reqwest::Method::GET, &header_path, None).await;
        assert_eq!(fetched.status, reqwest::StatusCode::OK);
        assert_eq!(fetched.body, record(&sample, 1));

        // A `POST` cannot pick its own id: an `id` key and an unknown key are both dropped and
        // the store numbers the record from the highest one it already holds.
        let second = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_extra_body),
        )
        .await;
        assert_eq!(second.status, reqwest::StatusCode::CREATED);
        assert_eq!(second.body, record(&sample, 2));
        assert_eq!(
            second.location.as_deref(),
            Some(by_id_path(&sample, 2).as_str())
        );

        // The list, and the file the list is read from, are the same two records in id order.
        let two = stored(&sample, &[1, 2]);
        let listed = send(
            &client,
            &server,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(listed.body, two);
        assert_eq!(store.read_store(&sample).as_deref(), Some(two.as_str()));

        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_body_rejections_are_the_frameworks_own_and_change_no_record() {
    // The bodies are chosen so the message is identical for both samples (`name` is the first
    // field of both DTOs, and serde reports the first failure only). `[]` is the exception: it
    // names the DTO type, so it is carried per sample in `reject_array_body`.
    const MISSING_NAME: &str = "Failed to deserialize the JSON body into the target type: missing field `name` at line 1 column 2";
    const NAME_IS_A_NUMBER: &str = "Failed to deserialize the JSON body into the target type: name: invalid type: integer `42`, expected a string at line 1 column 10";
    const TRUNCATED: &str = "Failed to parse the request body as JSON: name: EOF while parsing a value at line 1 column 9";
    const EMPTY_BODY: &str =
        "Failed to parse the request body as JSON: EOF while parsing a value at line 1 column 0";

    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-bad-bodies-{}", sample.store_dir));
        let server = TestServer::start(store.state()).await;
        let client = client();
        let one = by_id_path(&sample, 1);

        // One record on the shelf, so "changed nothing" is a statement about a real record
        // rather than about an empty store.
        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);
        let untouched = format!("[{}]", created.body);

        // Just over `server::MAX_REQUEST_BODY_BYTES`, i.e. the layer's answer, not a handler's.
        let oversized = format!(
            r#"{{"name": "{}"}}"#,
            "x".repeat(MAX_REQUEST_BODY_BYTES + 1024)
        );

        for (method, path, body, media_type, status, text) in [
            (
                reqwest::Method::POST,
                sample.collection,
                Some("{}"),
                Some("application/json"),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                MISSING_NAME,
            ),
            (
                reqwest::Method::POST,
                sample.collection,
                Some(r#"{"name":42}"#),
                Some("application/json"),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                NAME_IS_A_NUMBER,
            ),
            (
                reqwest::Method::POST,
                sample.collection,
                Some("[]"),
                Some("application/json"),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                sample.reject_array_body,
            ),
            (
                reqwest::Method::POST,
                sample.collection,
                Some(r#"{"name": "#),
                Some("application/json"),
                reqwest::StatusCode::BAD_REQUEST,
                TRUNCATED,
            ),
            (
                reqwest::Method::POST,
                sample.collection,
                Some(""),
                Some("application/json"),
                reqwest::StatusCode::BAD_REQUEST,
                EMPTY_BODY,
            ),
            (
                reqwest::Method::POST,
                sample.collection,
                Some(sample.post_body),
                Some("text/plain"),
                reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                REJECT_MEDIA_TYPE,
            ),
            (
                reqwest::Method::POST,
                sample.collection,
                Some(sample.post_body),
                None,
                reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                REJECT_MEDIA_TYPE,
            ),
            (
                reqwest::Method::POST,
                sample.collection,
                Some(oversized.as_str()),
                Some("application/json"),
                reqwest::StatusCode::PAYLOAD_TOO_LARGE,
                REJECT_BODY_TOO_LARGE,
            ),
            // The same DTOs back `PUT` and `PATCH`, so the same rules answer them; PATCH takes
            // a subset DTO, which is why `{}` is valid there but not for a whole-record `PUT`.
            (
                reqwest::Method::PUT,
                one.as_str(),
                Some("{}"),
                Some("application/json"),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                MISSING_NAME,
            ),
            (
                reqwest::Method::PATCH,
                one.as_str(),
                Some(r#"{"name":42}"#),
                Some("application/json"),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                NAME_IS_A_NUMBER,
            ),
        ] {
            let rejected = send_with_type(&client, &server, method, path, body, media_type).await;
            assert_eq!(
                rejected.status, status,
                "{status} expected for {path} with body {body:?} and media type {media_type:?}, got {:?}",
                rejected.status
            );
            // Plain text, not the handler's JSON: the request never reached the store.
            assert_eq!(rejected.content_type.as_deref(), Some(REJECT_TEXT_PLAIN));
            assert_eq!(rejected.body, text);
        }

        // None of the ten requests above moved a byte.
        let listed = send(
            &client,
            &server,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(listed.body, untouched);
        assert_eq!(
            store.read_store(&sample).as_deref(),
            Some(untouched.as_str())
        );

        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_an_unknown_id_is_404_with_that_id_in_the_message() {
    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-unknown-id-{}", sample.store_dir));
        let server = TestServer::start(store.state()).await;
        let client = client();
        let unknown = by_id_path(&sample, UNKNOWN_ID);
        let unknown_other = by_id_path(&sample, OTHER_UNKNOWN_ID);

        // All four verbs that take an id answer the same 404, with the id named.
        for (method, body) in [
            (reqwest::Method::GET, None),
            (reqwest::Method::DELETE, None),
            (reqwest::Method::PUT, Some(sample.post_body)),
            (reqwest::Method::PATCH, Some(r#"{"age":1}"#)),
        ] {
            // `Method` is not `Copy` and `send` consumes it, so the label is rendered first
            // (the rule the 405 loops below document as well).
            let label = format!("{method} {unknown}");
            let answer = send(&client, &server, method, &unknown, body).await;
            assert_eq!(
                answer.status,
                reqwest::StatusCode::NOT_FOUND,
                "{label} must be a 404 for an unknown id"
            );
            // JSON, i.e. the handler's own answer, not an extractor rejection.
            assert_eq!(answer.content_type.as_deref(), Some("application/json"));
            assert_eq!(answer.body, missing(&sample, UNKNOWN_ID));
        }

        // A second id proves the message interpolates rather than hardcodes.
        let other = send(&client, &server, reqwest::Method::GET, &unknown_other, None).await;
        assert_eq!(other.status, reqwest::StatusCode::NOT_FOUND);
        assert_eq!(other.body, missing(&sample, OTHER_UNKNOWN_ID));

        // Looking for a record that is not there created nothing either.
        assert_eq!(store.entries(), Vec::<String>::new());

        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_a_non_numeric_id_is_400_from_the_path_extractor() {
    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-bad-id-{}", sample.store_dir));
        let server = TestServer::start(store.state()).await;
        let client = client();

        // `Path<u64>` rejects before the handler runs, so the 400 wording is axum's and the
        // store is never opened: not a 404, which would mean a lookup happened.
        for segment in ["abc", "-1", "1.5", "18446744073709551616"] {
            let path = format!("{}/{segment}", sample.collection);
            for (method, body) in [
                (reqwest::Method::GET, None),
                (reqwest::Method::DELETE, None),
                (reqwest::Method::PUT, Some(sample.post_body)),
                (reqwest::Method::PATCH, Some(r#"{"age":1}"#)),
            ] {
                let label = format!("{method} {path}");
                let answer = send(&client, &server, method, &path, body).await;
                assert_eq!(
                    answer.status,
                    reqwest::StatusCode::BAD_REQUEST,
                    "{label} must be refused by the path extractor"
                );
                assert_eq!(answer.content_type.as_deref(), Some(REJECT_TEXT_PLAIN));
                assert_eq!(
                    answer.body,
                    format!("Invalid URL: Cannot parse `{segment}` to a `u64`")
                );
            }
        }

        // A percent-encoded segment is decoded BEFORE the parse, so the message names the
        // decoded value (` 1`, not `%201`) - the same rule the echo path parameter follows.
        let encoded = send(
            &client,
            &server,
            reqwest::Method::GET,
            &format!("{}/%201", sample.collection),
            None,
        )
        .await;
        assert_eq!(encoded.status, reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(encoded.body, "Invalid URL: Cannot parse ` 1` to a `u64`");

        assert_eq!(store.entries(), Vec::<String>::new());

        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_put_replaces_the_record_and_never_creates_a_missing_one() {
    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-put-{}", sample.store_dir));
        let server = TestServer::start(store.state()).await;
        let client = client();
        let one = by_id_path(&sample, 1);

        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);

        // A whole-record `PUT`, sent with an `id` in the body that the path has to win against.
        let replaced = send(
            &client,
            &server,
            reqwest::Method::PUT,
            &one,
            Some(sample.put_body),
        )
        .await;
        assert_eq!(replaced.status, reqwest::StatusCode::OK);
        assert_eq!(replaced.body, sample.put_result);

        // Replacement, not append and not a second record: one record, the new field values.
        let listed = send(
            &client,
            &server,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(listed.body, format!("[{}]", sample.put_result));
        assert_eq!(
            store.read_store(&sample).as_deref(),
            Some(listed.body.as_str())
        );
        let gone = send(&client, &server, reqwest::Method::GET, &one, None).await;
        assert_eq!(gone.body, sample.put_result);

        // `PUT` on an unknown id is a 404 and does NOT insert, so the store still holds the
        // record it had - byte for byte.
        let unknown = by_id_path(&sample, UNKNOWN_ID);
        let not_created = send(
            &client,
            &server,
            reqwest::Method::PUT,
            &unknown,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(not_created.status, reqwest::StatusCode::NOT_FOUND);
        assert_eq!(not_created.body, missing(&sample, UNKNOWN_ID));
        let still = send(
            &client,
            &server,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(still.body, format!("[{}]", sample.put_result));
        assert_eq!(
            store.read_store(&sample).as_deref(),
            Some(still.body.as_str())
        );

        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_patch_writes_only_the_fields_it_is_sent() {
    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-patch-{}", sample.store_dir));
        let server = TestServer::start(store.state()).await;
        let client = client();
        let one = by_id_path(&sample, 1);

        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);

        // `{}` is a valid body that changes nothing, and an explicit `null` means the same as
        // an absent key: keep the stored value. `id` cannot renumber a record.
        for (body, expected) in [
            ("{}", record(&sample, 1).as_str()),
            (sample.patch_body, sample.patch_result),
            (sample.patch_null_body, sample.patch_null_result),
            (sample.patch_extra_body, sample.patch_extra_result),
        ] {
            let patched = send(&client, &server, reqwest::Method::PATCH, &one, Some(body)).await;
            assert_eq!(
                patched.status,
                reqwest::StatusCode::OK,
                "PATCH with body {body:?} must answer 200"
            );
            assert_eq!(
                patched.body, expected,
                "PATCH with body {body:?} changed more than it was sent"
            );

            // The answer and the stored record agree, so the next step really starts here.
            let fetched = send(&client, &server, reqwest::Method::GET, &one, None).await;
            assert_eq!(fetched.body, expected);
        }

        assert_eq!(
            store.read_store(&sample).as_deref(),
            Some(format!("[{}]", sample.patch_extra_result).as_str())
        );

        // PATCH on an unknown id is the same 404 as every other verb, and writes nothing.
        let unknown = by_id_path(&sample, UNKNOWN_ID);
        let not_found = send(
            &client,
            &server,
            reqwest::Method::PATCH,
            &unknown,
            Some(sample.patch_body),
        )
        .await;
        assert_eq!(not_found.status, reqwest::StatusCode::NOT_FOUND);
        assert_eq!(not_found.body, missing(&sample, UNKNOWN_ID));
        assert_eq!(
            store.read_store(&sample).as_deref(),
            Some(format!("[{}]", sample.patch_extra_result).as_str())
        );

        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_delete_answers_204_with_no_body_and_a_second_delete_is_404() {
    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-delete-{}", sample.store_dir));
        let server = TestServer::start(store.state()).await;
        let client = client();
        let one = by_id_path(&sample, 1);

        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);

        let deleted = send(&client, &server, reqwest::Method::DELETE, &one, None).await;
        assert_eq!(deleted.status, reqwest::StatusCode::NO_CONTENT);
        // A 204 has no body to describe, so it must not name a media type either, and it does
        // not echo the record it removed.
        assert_eq!(deleted.body, "");
        assert_eq!(deleted.content_type, None);

        // The record is gone from every view: read, list, and the file behind both.
        let fetched = send(&client, &server, reqwest::Method::GET, &one, None).await;
        assert_eq!(fetched.status, reqwest::StatusCode::NOT_FOUND);
        let listed = send(
            &client,
            &server,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(listed.body, "[]");
        assert_eq!(store.read_store(&sample).as_deref(), Some("[]"));

        // Deleting the same id again is a 404 (there is nothing to remove a second time).
        let again = send(&client, &server, reqwest::Method::DELETE, &one, None).await;
        assert_eq!(again.status, reqwest::StatusCode::NOT_FOUND);
        assert_eq!(again.body, missing(&sample, 1));
        assert_eq!(store.read_store(&sample).as_deref(), Some("[]"));

        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_ids_are_the_highest_stored_id_plus_one() {
    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-id-rule-{}", sample.store_dir));
        let server = TestServer::start(store.state()).await;
        let client = client();

        // The rule is `max(stored id) + 1`, so an id is recycled exactly when the record that
        // held the HIGHEST id is deleted - a hole lower down stays a hole. This is the sequence
        // that pins both halves of that sentence.
        for next in 1..=3 {
            let created = send(
                &client,
                &server,
                reqwest::Method::POST,
                sample.collection,
                Some(sample.post_body),
            )
            .await;
            assert_eq!(created.status, reqwest::StatusCode::CREATED);
            assert_eq!(
                created.body,
                record(&sample, next),
                "create #{next} must hand out id {next}"
            );
            assert_eq!(
                created.location.as_deref(),
                Some(by_id_path(&sample, next).as_str())
            );
        }

        // 1, 2, 3 exist. Delete the middle one, and the next create skips over the hole.
        let middle = by_id_path(&sample, 2);
        let deleted = send(&client, &server, reqwest::Method::DELETE, &middle, None).await;
        assert_eq!(deleted.status, reqwest::StatusCode::NO_CONTENT);
        let after_hole = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(
            after_hole.body,
            record(&sample, 4),
            "id 2 is not reusable while a higher id is still stored"
        );

        // Delete the highest record and the id it held is handed straight back.
        let top = by_id_path(&sample, 4);
        let deleted_top = send(&client, &server, reqwest::Method::DELETE, &top, None).await;
        assert_eq!(deleted_top.status, reqwest::StatusCode::NO_CONTENT);
        let reused = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(reused.body, record(&sample, 4));
        assert_eq!(
            reused.location.as_deref(),
            Some(by_id_path(&sample, 4).as_str())
        );

        // Nothing renumbers the survivors, so the stored array keeps its holes: 1, 3, 4.
        assert_eq!(
            store.read_store(&sample).as_deref(),
            Some(stored(&sample, &[1, 3, 4]).as_str())
        );
        let listed = send(
            &client,
            &server,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(listed.body, stored(&sample, &[1, 3, 4]));

        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_records_survive_a_new_app_state_on_the_same_data_root() {
    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-restart-{}", sample.store_dir));
        let client = client();

        let server = TestServer::start(store.state()).await;
        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);
        let second = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(second.body, record(&sample, 2));
        let patched = send(
            &client,
            &server,
            reqwest::Method::PATCH,
            &by_id_path(&sample, 1),
            Some(sample.patch_body),
        )
        .await;
        assert_eq!(patched.body, sample.patch_result);
        let deleted = send(
            &client,
            &server,
            reqwest::Method::DELETE,
            &by_id_path(&sample, 2),
            None,
        )
        .await;
        assert_eq!(deleted.status, reqwest::StatusCode::NO_CONTENT);
        // This is the whole store as the first process leaves it: one edited record, and the
        // id sequence stopped at 2 because 2 is no longer held by anything.
        let left_behind = format!("[{}]", sample.patch_result);
        assert_eq!(
            store.read_store(&sample).as_deref(),
            Some(left_behind.as_str())
        );
        server.shutdown().await;

        // A fresh `AppState` over the same root - what `src/bin/server.rs` builds on every
        // start - reads exactly those bytes and continues exactly that sequence: the file IS
        // the store, so a restart survives.
        let restarted = TestServer::start(store.state()).await;
        let listed = send(
            &client,
            &restarted,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(listed.body, left_behind);
        let fetched = send(
            &client,
            &restarted,
            reqwest::Method::GET,
            &by_id_path(&sample, 1),
            None,
        )
        .await;
        assert_eq!(fetched.body, sample.patch_result);
        let removed = send(
            &client,
            &restarted,
            reqwest::Method::GET,
            &by_id_path(&sample, 2),
            None,
        )
        .await;
        assert_eq!(removed.status, reqwest::StatusCode::NOT_FOUND);
        assert_eq!(removed.body, missing(&sample, 2));
        let fresh = send(
            &client,
            &restarted,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(
            fresh.body,
            record(&sample, 2),
            "a restart must not restart the numbering below the highest stored id"
        );
        restarted.shutdown().await;
    }
}

#[tokio::test]
async fn rest_a_store_file_that_does_not_parse_is_a_500_left_byte_identical() {
    for sample in rest_samples() {
        // Two shapes of "unparseable": not JSON at all, and JSON whose records do not fit the
        // type. Both are the same single 500, and neither is "healed".
        for junk in [sample.corrupt_store, sample.wrong_shape_store] {
            let store = StoreFixture::new(&format!("rest-corrupt-{}", sample.store_dir));
            store.write_store(&sample, junk);
            let server = TestServer::start(store.state()).await;
            let client = client();
            let one = by_id_path(&sample, 1);

            for (method, path, body) in [
                (reqwest::Method::GET, sample.collection, None),
                (
                    reqwest::Method::POST,
                    sample.collection,
                    Some(sample.post_body),
                ),
                (reqwest::Method::GET, one.as_str(), None),
                (reqwest::Method::PUT, one.as_str(), Some(sample.post_body)),
                (reqwest::Method::PATCH, one.as_str(), Some("{}")),
                (reqwest::Method::DELETE, one.as_str(), None),
            ] {
                let label = format!("{method} {path}");
                let answer = send(&client, &server, method, path, body).await;
                assert_eq!(
                    answer.status,
                    reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    "{label} must not guess its way past an unreadable store"
                );
                // The handler's own JSON, and the one message the route file can produce: no
                // filesystem path, no parser detail.
                assert_eq!(answer.content_type.as_deref(), Some("application/json"));
                assert_eq!(answer.body, sample.store_error_body);
            }

            // The bytes on disk are exactly what this test wrote.
            assert_eq!(store.read_store(&sample).as_deref(), Some(junk));

            server.shutdown().await;
        }

        // The contrast that makes the rule readable: an empty ARRAY is not corrupt.
        let store = StoreFixture::new(&format!("rest-empty-array-{}", sample.store_dir));
        store.write_store(&sample, "[]");
        let server = TestServer::start(store.state()).await;
        let listed = send(
            &client(),
            &server,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(listed.status, reqwest::StatusCode::OK);
        assert_eq!(listed.body, "[]");
        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_reads_create_nothing_and_the_first_write_creates_the_folder_and_file() {
    for sample in rest_samples() {
        let store = StoreFixture::new(&format!("rest-lazy-{}", sample.store_dir));
        let server = TestServer::start(store.state()).await;
        let client = client();
        let unknown = by_id_path(&sample, UNKNOWN_ID);

        // Every read and every refused write, each with the status the contract gives it. None
        // of them may create anything: neither the resource folder nor its file is created
        // eagerly, and a 404 is a lookup, not a write.
        for (method, path, body, status) in [
            (
                reqwest::Method::GET,
                sample.collection,
                None,
                reqwest::StatusCode::OK,
            ),
            (
                reqwest::Method::GET,
                unknown.as_str(),
                None,
                reqwest::StatusCode::NOT_FOUND,
            ),
            (
                reqwest::Method::PUT,
                unknown.as_str(),
                Some(sample.post_body),
                reqwest::StatusCode::NOT_FOUND,
            ),
            (
                reqwest::Method::PATCH,
                unknown.as_str(),
                Some("{}"),
                reqwest::StatusCode::NOT_FOUND,
            ),
            (
                reqwest::Method::DELETE,
                unknown.as_str(),
                None,
                reqwest::StatusCode::NOT_FOUND,
            ),
            (
                reqwest::Method::POST,
                sample.collection,
                Some("{}"),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                reqwest::Method::GET,
                sample.collection,
                None,
                reqwest::StatusCode::OK,
            ),
        ] {
            let label = format!("{method} {path}");
            let answer = send(&client, &server, method, path, body).await;
            assert_eq!(
                answer.status, status,
                "{label} was expected to answer {status}"
            );
        }
        // A `text/plain` POST is refused by the framework, and still creates nothing.
        let refused = send_with_type(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
            Some("text/plain"),
        )
        .await;
        assert_eq!(refused.status, reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(store.entries(), Vec::<String>::new());

        // The first accepted write creates the folder and the file, and the file is the whole
        // array in the exact shape the list answers with.
        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);
        assert_eq!(store.entries(), [sample.store_dir]);
        assert_eq!(
            store.read_store(&sample).as_deref(),
            Some(stored(&sample, &[1]).as_str())
        );

        server.shutdown().await;
    }
}

#[tokio::test]
async fn rest_samples_share_one_data_root_without_sharing_records_or_ids() {
    let store = StoreFixture::new("rest-two-resources");
    let server = TestServer::start(store.state()).await;
    let client = client();
    let [pets_sample, locations_sample] = rest_samples();

    // Both first records are id 1 on the SAME root: the sequences are per resource, and one
    // resource's file is never another's.
    for sample in [pets_sample, locations_sample] {
        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);
        assert_eq!(created.body, record(&sample, 1));
        assert_eq!(
            created.location.as_deref(),
            Some(by_id_path(&sample, 1).as_str())
        );
    }
    for sample in [pets_sample, locations_sample] {
        let listed = send(
            &client,
            &server,
            reqwest::Method::GET,
            sample.collection,
            None,
        )
        .await;
        assert_eq!(listed.body, stored(&sample, &[1]));
        assert_eq!(
            store.read_store(&sample).as_deref(),
            Some(stored(&sample, &[1]).as_str())
        );
    }
    assert_eq!(store.entries(), ["locations", "pets"]);

    // Removing the pet leaves the location with the same id and the same bytes.
    let deleted = send(
        &client,
        &server,
        reqwest::Method::DELETE,
        &by_id_path(&pets_sample, 1),
        None,
    )
    .await;
    assert_eq!(deleted.status, reqwest::StatusCode::NO_CONTENT);
    let pets_now = send(
        &client,
        &server,
        reqwest::Method::GET,
        pets_sample.collection,
        None,
    )
    .await;
    assert_eq!(pets_now.body, "[]");
    assert_eq!(store.read_store(&pets_sample).as_deref(), Some("[]"));
    let locations_now = send(
        &client,
        &server,
        reqwest::Method::GET,
        locations_sample.collection,
        None,
    )
    .await;
    assert_eq!(locations_now.body, stored(&locations_sample, &[1]));
    assert_eq!(
        store.read_store(&locations_sample).as_deref(),
        Some(stored(&locations_sample, &[1]).as_str())
    );

    server.shutdown().await;
}

#[tokio::test]
async fn locations_city_filter_matches_the_whole_value_and_nothing_else() {
    let sample = locations();
    let store = StoreFixture::new("locations-city-filter");
    let server = TestServer::start(store.state()).await;
    let client = client();

    // Two records in Berlin and one in Hamburg, so a filter has something to narrow and
    // something to leave out.
    let hamburg = r#"{"name":"Warehouse","city":"Hamburg","country":"DE"}"#;
    let cafe = r#"{"name":"Cafe","city":"Berlin","country":"DE"}"#;
    let second = r#"{"id":2,"name":"Warehouse","city":"Hamburg","country":"DE"}"#;
    let third = r#"{"id":3,"name":"Cafe","city":"Berlin","country":"DE"}"#;
    for body in [sample.post_body, hamburg, cafe] {
        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);
    }
    let all = format!("[{},{},{third}]", record(&sample, 1), second);
    let berlins = format!("[{},{}]", record(&sample, 1), third);

    // Only a value that equals `city` byte for byte narrows anything.
    for (query, expected) in [
        ("", all.as_str()),
        ("?city=Berlin", berlins.as_str()),
        ("?city=berlin", "[]"),
        ("?city=Ber", "[]"),
        ("?city=%20Berlin", "[]"),
        ("?city=Berlin%20", "[]"),
        ("?city=Nowhere", "[]"),
        ("?city=", all.as_str()),
        ("?lang=en", all.as_str()),
        ("?CITY=Berlin", all.as_str()),
        ("?city=Berlin&lang=en", berlins.as_str()),
    ] {
        let listed = send(
            &client,
            &server,
            reqwest::Method::GET,
            &format!("{}{query}", sample.collection),
            None,
        )
        .await;
        assert_eq!(listed.status, reqwest::StatusCode::OK);
        assert_eq!(
            listed.body, expected,
            "GET {query} answered the wrong subset"
        );
    }

    // A repeated key is the one query shape the DTO refuses, and it does so as an ordinary
    // 400 from the `Query` extractor rather than guessing one of the two values.
    let duplicate = send(
        &client,
        &server,
        reqwest::Method::GET,
        &format!("{}?city=Berlin&city=Hamburg", sample.collection),
        None,
    )
    .await;
    assert_eq!(duplicate.status, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(duplicate.content_type.as_deref(), Some(REJECT_TEXT_PLAIN));
    assert_eq!(
        duplicate.body,
        "Failed to deserialize query string: duplicate field `city`"
    );

    // Filtering is a read: the store still holds all three records.
    assert_eq!(store.read_store(&sample).as_deref(), Some(all.as_str()));

    server.shutdown().await;
}

#[tokio::test]
async fn pets_list_ignores_every_query_parameter_it_does_not_declare() {
    let sample = pets();
    let store = StoreFixture::new("pets-query-ignored");
    let server = TestServer::start(store.state()).await;
    let client = client();
    for _ in 0..2 {
        let created = send(
            &client,
            &server,
            reqwest::Method::POST,
            sample.collection,
            Some(sample.post_body),
        )
        .await;
        assert_eq!(created.status, reqwest::StatusCode::CREATED);
    }
    let both = stored(&sample, &[1, 2]);

    // The pets handler takes no `Query` extractor at all, so nothing here can reject or
    // narrow: not a foreign key, not an empty value, and (unlike `locations`) not even a
    // repeated key.
    for query in [
        "?city=Berlin",
        "?city=",
        "?city=Berlin&lang=en",
        "?city=Berlin&city=Hamburg",
        "?",
    ] {
        let listed = send(
            &client,
            &server,
            reqwest::Method::GET,
            &format!("{}{query}", sample.collection),
            None,
        )
        .await;
        assert_eq!(listed.status, reqwest::StatusCode::OK);
        assert_eq!(listed.body, both, "GET {query} was expected to be ignored");
    }
    let fetched = send(
        &client,
        &server,
        reqwest::Method::GET,
        &format!("{}?city=Berlin", by_id_path(&sample, 1)),
        None,
    )
    .await;
    assert_eq!(fetched.body, record(&sample, 1));

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// The builtin browser surface (T18): the embedded playground and the endpoint
// listing it navigates by.
//
// The paths below are spelled as wire literals rather than imported from
// `server::{PLAYGROUND_PATH, ROUTE_METADATA_PATH, ..}` on purpose: a builtin path
// is part of the published HTTP contract (the page hard-codes `/playground.css`
// and `/playground.js`, and the script hard-codes `/v1/services/routes`), so
// renaming one has to fail a test instead of silently moving both sides.
// ---------------------------------------------------------------------------

/// The exact `Content-Security-Policy` every builtin response carries, pinned as
/// the bytes a browser enforces (not as a "contains `default-src`" approximation).
///
/// It grants no `'unsafe-inline'`, which is only honest because the document has
/// neither an inline script nor an inline style element - the two asset assertions
/// below are what keep that claim true.
const PLAYGROUND_CSP: &str = "default-src 'self'; img-src 'self' data:; script-src 'self'; style-src 'self'; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'";

/// The whole listing, exactly as the generated table serialises it today: the fifteen sample
/// endpoints, in order, with the summaries their route files author.
///
/// Two orders combine into this sequence, and both are pinned elsewhere:
///   * between files it is the scanner's registration order (byte-wise comparison
///     of the scan-root-relative path, `build_support/discovery.rs`, asserted in
///     `tests/discovery.rs`), which puts `endpoints/echo/…` before `endpoints/health/…`,
///     then `endpoints/locations/…` and finally `endpoints/pets/…`;
///   * inside one file it is the order the entries are written in
///     `crate::service_routes!` (`src/server/metadata.rs:128-160`), concatenated
///     verbatim by the generated `route_metadata` (`build_support/emit.rs:172-188`,
///     `DISCOVERED.concat()`).
///
/// So `POST /v1/services/echo` precedes `GET /v1/services/echo/{value}` because
/// that is how the echo route file lists them - the README wire table is ordered by
/// resource, this one is ordered by registration. The two REST samples each contribute the
/// same six rows in the same order (collection `GET`/`POST`, then `{id}`
/// `GET`/`PUT`/`PATCH`/`DELETE`), which is the order their route files declare them.
///
/// Like `health_body`, this is a value edited on purpose: adding a sample endpoint
/// or rewording a summary changes the listing, and the listing is asserted exactly.
fn route_table() -> JsonValue {
    json!([
        {
            "method": "POST",
            "path": "/v1/services/echo",
            "summary": "Echoes the posted `value` unchanged",
        },
        {
            "method": "GET",
            "path": "/v1/services/echo/{value}",
            "summary": "Echoes `{value}`, with `?prefix=` applied",
        },
        {
            "method": "GET",
            "path": "/v1/services/health",
            "summary": "Reports the service name and version",
        },
        {
            "method": "GET",
            "path": "/v1/services/locations",
            "summary": "Lists every stored location, `?city=` narrows by exact city",
        },
        {
            "method": "POST",
            "path": "/v1/services/locations",
            "summary": "Creates a location and answers 201 with it",
        },
        {
            "method": "GET",
            "path": "/v1/services/locations/{id}",
            "summary": "Returns one location by id, or 404 when the id is unknown",
        },
        {
            "method": "PUT",
            "path": "/v1/services/locations/{id}",
            "summary": "Replaces one location by id, or 404 when the id is unknown",
        },
        {
            "method": "PATCH",
            "path": "/v1/services/locations/{id}",
            "summary": "Updates only the sent fields of one location, or 404 when the id is unknown",
        },
        {
            "method": "DELETE",
            "path": "/v1/services/locations/{id}",
            "summary": "Removes one location by id and answers 204, or 404 when the id is unknown",
        },
        {
            "method": "GET",
            "path": "/v1/services/pets",
            "summary": "Lists every stored pet",
        },
        {
            "method": "POST",
            "path": "/v1/services/pets",
            "summary": "Creates a pet and answers 201 with it",
        },
        {
            "method": "GET",
            "path": "/v1/services/pets/{id}",
            "summary": "Returns one pet by id, or 404 when the id is unknown",
        },
        {
            "method": "PUT",
            "path": "/v1/services/pets/{id}",
            "summary": "Replaces one pet by id, or 404 when the id is unknown",
        },
        {
            "method": "PATCH",
            "path": "/v1/services/pets/{id}",
            "summary": "Updates only the sent fields of one pet, or 404 when the id is unknown",
        },
        {
            "method": "DELETE",
            "path": "/v1/services/pets/{id}",
            "summary": "Removes one pet by id and answers 204, or 404 when the id is unknown",
        },
    ])
}

/// The stand-in for every `{param}` in a listed template.
const PARAM_PROBE_VALUE: &str = "probe";

/// The `{id}` stand-in has to be a number, and `1` is the id the sample stores hand out first,
/// so the row that creates a record is followed by the rows that address it.
const PARAM_PROBE_ID: &str = "1";

/// One header lookup: absent and non-UTF-8 both read as `None`, so a wrong header
/// value fails the comparison instead of the conversion.
fn header_value(response: &reqwest::Response, name: HeaderName) -> Option<&str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

/// The header set every builtin (page, stylesheet, script, listing) must carry,
/// asserted as exact values. `content_type` is the one per-response difference.
fn assert_builtin_headers(response: &reqwest::Response, content_type: &str) {
    assert_eq!(
        header_value(response, CONTENT_TYPE),
        Some(content_type),
        "unexpected content type on {}",
        response.url().path()
    );
    // `no-store`: the assets are compiled into the binary, so a cached copy is a
    // different version of the tool (a stale script with a fresh page is the
    // failure mode this exists to prevent).
    assert_eq!(header_value(response, CACHE_CONTROL), Some("no-store"));
    assert_eq!(
        header_value(response, X_CONTENT_TYPE_OPTIONS),
        Some("nosniff")
    );
    assert_eq!(header_value(response, REFERRER_POLICY), Some("no-referrer"));
    assert_eq!(
        header_value(response, CONTENT_SECURITY_POLICY),
        Some(PLAYGROUND_CSP)
    );
}

/// No response body may identify this checkout: the data root is already pinned
/// that way, and the embedded assets are the first place a `env!`-style path could
/// leak (an `include_str!` comment naming the scanner tree, for instance).
fn assert_no_source_leak(text: &str, what: &str) {
    for needle in [
        env!("CARGO_MANIFEST_DIR"),
        "packages/typecrate",
        r"packages\typecrate",
        "examples/endpoints",
        // The example file names themselves: a listing row or an asset comment that quotes
        // `<resource>-route.rs` / `<resource>-core.rs` leaks the checkout's layout the same
        // way a path would, and the scanned layout is not part of the wire contract.
        "-route",
        "-core",
        "Temp/",
    ] {
        assert!(
            !text.contains(needle),
            "{what} leaked {needle:?} into the response: {text}"
        );
    }
}

/// Parses a listing body into `(method, path, summary)` triples, pinning the exact
/// per-entry shape on the way: `method`, `path`, `summary`, nothing else.
///
/// The key-order assertion holds under both `serde_json` map implementations
/// (a plain `Map` is a `BTreeMap`, so the keys come back sorted, and
/// `method < path < summary` byte-wise; with `preserve_order` it is the struct's
/// field order, which is the same sequence).
fn listing_entries(body: &JsonValue) -> Vec<(&str, &str, &str)> {
    let entries = body
        .as_array()
        .unwrap_or_else(|| panic!("the listing is not a JSON array: {body}"));
    entries
        .iter()
        .map(|entry| {
            let object = entry
                .as_object()
                .unwrap_or_else(|| panic!("a listing entry is not an object: {entry}"));
            let keys: Vec<&str> = object.keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                ["method", "path", "summary"],
                "unexpected listing entry shape: {entry}"
            );
            let method = object
                .get("method")
                .and_then(JsonValue::as_str)
                .unwrap_or_else(|| panic!("`method` is not a string: {entry}"));
            let path = object
                .get("path")
                .and_then(JsonValue::as_str)
                .unwrap_or_else(|| panic!("`path` is not a string: {entry}"));
            let summary = object
                .get("summary")
                .and_then(JsonValue::as_str)
                .unwrap_or_else(|| panic!("`summary` is not a string: {entry}"));
            (method, path, summary)
        })
        .collect()
}

#[tokio::test]
async fn root_serves_the_embedded_playground_document() {
    let server = TestServer::start(AppState::new(scratch_root("playground-root"))).await;

    let response = client()
        .get(server.url("/"))
        .send()
        .await
        // `GET /` used to be a 404; the tool is what makes the service usable from
        // a browser on the LAN, so this is the request the whole surface exists for.
        .expect("the playground is reachable at the root");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_builtin_headers(&response, "text/html; charset=utf-8");
    let document = response.text().await.expect("the page has a body");

    // A complete document, not a fragment: these are the markers the layout, the
    // accessibility story and the CSP are all built on.
    assert!(
        document.starts_with("<!doctype html>"),
        "the page is not an HTML document: {document}"
    );
    for marker in [
        "<html lang=\"en\">",
        "<meta charset=\"utf-8\">",
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
        "<link rel=\"stylesheet\" href=\"/playground.css\">",
        "<script src=\"/playground.js\"></script>",
        "id=\"endpoint-search\"",
        "id=\"request-form\"",
        "id=\"request-body\"",
        "id=\"response-body\"",
    ] {
        assert!(
            document.contains(marker),
            "the playground document is missing {marker:?}"
        );
    }

    // Keeping the CSP honest: exactly one script element (the asset), no style
    // element, and no absolute URL in any `src`/`href` - the favicon is a `data:`
    // URI precisely so that stays true.
    assert_eq!(
        document.matches("<script").count(),
        1,
        "the page must reference exactly one script: {document}"
    );
    assert!(
        !document.contains("<style"),
        "the page must carry no inline stylesheet"
    );
    assert!(
        !document.contains("href=\"http"),
        "the page must not link outside this origin"
    );
    assert!(
        !document.contains("src=\"http"),
        "the page must not load an external script"
    );
    assert_no_source_leak(&document, "the playground document");

    server.shutdown().await;
}

#[tokio::test]
async fn playground_assets_are_served_as_same_origin_documents() {
    let server = TestServer::start(AppState::new(scratch_root("playground-assets"))).await;
    let client = client();

    let stylesheet = client
        .get(server.url("/playground.css"))
        .send()
        .await
        .expect("the stylesheet is served");
    assert_eq!(stylesheet.status(), reqwest::StatusCode::OK);
    assert_builtin_headers(&stylesheet, "text/css; charset=utf-8");
    let css = stylesheet.text().await.expect("the stylesheet has a body");
    // The class/attribute selectors `playground.html` and `playground.js` depend
    // on live in the same build as the page that uses them.
    for marker in [
        ".endpoint-list",
        ".badge[data-method=\"get\"]",
        ".status-pill[data-tone=\"error\"]",
        "prefers-color-scheme: dark",
        "min-width: 900px",
        "min-width: 1280px",
        "max-width: 480px",
    ] {
        assert!(css.contains(marker), "the stylesheet is missing {marker:?}");
    }
    assert!(
        !css.contains("@import") && !css.contains("url(") && !css.contains("http"),
        "the stylesheet must fetch nothing outside this origin"
    );
    assert_no_source_leak(&css, "the playground stylesheet");

    let script = client
        .get(server.url("/playground.js"))
        .send()
        .await
        .expect("the script is served");
    assert_eq!(script.status(), reqwest::StatusCode::OK);
    assert_builtin_headers(&script, "text/javascript; charset=utf-8");
    let js = script.text().await.expect("the script has a body");
    // The same-origin and text-only rules are the security contract of this page,
    // so the constructs that implement them are pinned by name.
    for marker in [
        "\"/v1/services/routes\"",
        "mode: \"same-origin\"",
        "redirect: \"error\"",
        "credentials: \"same-origin\"",
        "URLSearchParams",
        "encodeURIComponent",
        "AbortController",
        "textContent",
        "createElement",
    ] {
        assert!(js.contains(marker), "the script is missing {marker:?}");
    }
    for banned in [
        "innerHTML",
        "document.write",
        "eval(",
        "http://",
        "https://",
    ] {
        assert!(
            !js.contains(banned),
            "the script must not use {banned:?} (untrusted text, same-origin URLs)"
        );
    }
    assert_no_source_leak(&js, "the playground script");

    // A `GET` route also answers `HEAD` with the same headers and no body (axum
    // `src/routing/method_routing.rs:55`), which is the body-less case the page's
    // response rendering has to survive.
    let head = client
        .head(server.url("/"))
        .send()
        .await
        .expect("HEAD is answered at the root");
    assert_eq!(head.status(), reqwest::StatusCode::OK);
    assert_builtin_headers(&head, "text/html; charset=utf-8");
    assert_eq!(
        head.text()
            .await
            .expect("a HEAD response has a readable body"),
        ""
    );

    server.shutdown().await;
}

#[tokio::test]
async fn routes_metadata_lists_exactly_the_generated_endpoints() {
    let server = TestServer::start(AppState::new(scratch_root("routes-meta"))).await;

    let response = client()
        .get(server.url("/v1/services/routes"))
        .send()
        .await
        .expect("the endpoint listing is reachable");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    // Exact, not a prefix: `Json` inserts this header last (axum-0.8.9
    // `src/json.rs:205-216`, `mime::APPLICATION_JSON` == `application/json`), and
    // UTF-8 JSON needs no `charset` parameter (RFC 8259 section 8.1).
    assert_builtin_headers(&response, "application/json");

    let status = response.status();
    let raw = response.text().await.expect("the listing has a body");
    let body: JsonValue = serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("status {status} listing body is not JSON: {raw:?} ({err})"));
    assert_no_source_leak(&raw, "the endpoint listing");

    let entries = listing_entries(&body);

    // One comparison pins the count, the order, the per-entry key set and every
    // string, summaries included: a `Map` compares by contents, so an added or
    // missing field on any entry fails here as well. `listing_entries` above is
    // what turns that into a message naming the entry.
    assert_eq!(
        body,
        route_table(),
        "the listing is not the generated table: {raw}"
    );
    assert_eq!(
        entries.len(),
        15,
        "the listing did not report the fifteen sample endpoints: {raw}"
    );

    // Built-ins are absent from their own listing by construction: the generated
    // table walks the discovered modules only (`build_support/emit.rs:168-171`).
    // Asserted separately because the failure mode is the interesting one - a row
    // for `/` would mean the generator started describing axum's registrations
    // instead of the endpoint tree, and the listing would stop answering "which
    // endpoints does this build compile in".
    for builtin in [
        "/",
        "/playground.css",
        "/playground.js",
        "/v1/services/routes",
    ] {
        assert!(
            !entries.iter().any(|&(_, listed, _)| listed == builtin),
            "the listing advertises the builtin path {builtin:?}: {raw}"
        );
    }

    // The exact table above fixes the summary wording (it is authored in the route
    // files); these rules are the part that must survive any rewording, because a
    // summary is rendered as list text in the playground and a source path in it
    // would be an information leak (R4).
    for &(method, path, summary) in &entries {
        assert!(
            !summary.trim().is_empty(),
            "{method} {path} is listed without a summary"
        );
        assert!(
            summary.chars().any(char::is_alphabetic),
            "{method} {path} summary is not a label: {summary:?}"
        );
        for needle in ["\\", ".rs", ":/", "packages", "examples", "Temp"] {
            assert!(
                !summary.contains(needle),
                "{method} {path} summary leaks {needle:?}: {summary:?}"
            );
        }
    }

    server.shutdown().await;
}

/// One row of the listing, plus the exact answer the socket owes for it.
///
/// Rows are dispatched IN LISTING ORDER against one fresh data root, and that order happens to
/// be a working CRUD walk per sample: the collection `GET` sees the empty store (`[]`), the
/// `POST` creates id 1 (which is what `PARAM_PROBE_ID` then addresses), and `PUT`/`PATCH`/
/// `DELETE` follow. So the expectation of a later row is the store the earlier rows left
/// behind - spelled out per row below, because that chaining is itself part of what this test
/// proves: the listing describes endpoints that really cooperate in the order it advertises.
struct Dispatch {
    /// The advertised method and path template; both are asserted against the listing row this
    /// one belongs to, before anything is sent.
    method: &'static str,
    template: &'static str,
    /// Request body text; `None` sends no body.
    body: Option<&'static str>,
    status: reqwest::StatusCode,
    /// The raw body bytes. Empty for the `204`.
    answer: String,
    /// The `location` header, which only a `201` may send.
    location: Option<String>,
}

/// Shorthand so the table below stays one row per line.
fn dispatch(
    method: &'static str,
    template: &'static str,
    body: Option<&'static str>,
    status: reqwest::StatusCode,
    answer: impl Into<String>,
    location: Option<String>,
) -> Dispatch {
    Dispatch {
        method,
        template,
        body,
        status,
        answer: answer.into(),
        location,
    }
}

/// The fifteen rows of `route_table`, in that order, each with its contract answer.
fn dispatch_plan() -> [Dispatch; 15] {
    let pets = pets();
    let locations = locations();
    [
        dispatch(
            "POST",
            "/v1/services/echo",
            Some(r#"{"value":"probe"}"#),
            reqwest::StatusCode::OK,
            r#"{"value":"probe"}"#,
            None,
        ),
        // Both echo variants answer the probe: the body one echoes the posted value, the path
        // one echoes the `{value}` segment.
        dispatch(
            "GET",
            "/v1/services/echo/{value}",
            None,
            reqwest::StatusCode::OK,
            r#"{"value":"probe"}"#,
            None,
        ),
        dispatch(
            "GET",
            "/v1/services/health",
            None,
            reqwest::StatusCode::OK,
            r#"{"service":"typecrate-services","version":"1.0.2"}"#,
            None,
        ),
        // locations: nothing stored yet, then the full walk on the record this row creates.
        dispatch(
            "GET",
            "/v1/services/locations",
            None,
            reqwest::StatusCode::OK,
            "[]",
            None,
        ),
        dispatch(
            "POST",
            "/v1/services/locations",
            Some(locations.post_body),
            reqwest::StatusCode::CREATED,
            record(&locations, 1),
            Some(by_id_path(&locations, 1)),
        ),
        dispatch(
            "GET",
            "/v1/services/locations/{id}",
            None,
            reqwest::StatusCode::OK,
            record(&locations, 1),
            None,
        ),
        dispatch(
            "PUT",
            "/v1/services/locations/{id}",
            Some(locations.put_body),
            reqwest::StatusCode::OK,
            locations.put_result,
            None,
        ),
        // The `PATCH` lands on the record the `PUT` left behind, so only its `city` moves.
        dispatch(
            "PATCH",
            "/v1/services/locations/{id}",
            Some(locations.patch_body),
            reqwest::StatusCode::OK,
            r#"{"id":1,"name":"Warehouse","city":"Munich","country":"DE"}"#,
            None,
        ),
        dispatch(
            "DELETE",
            "/v1/services/locations/{id}",
            None,
            reqwest::StatusCode::NO_CONTENT,
            "",
            None,
        ),
        // pets: the same walk, and its own empty store (the samples do not share records).
        dispatch(
            "GET",
            "/v1/services/pets",
            None,
            reqwest::StatusCode::OK,
            "[]",
            None,
        ),
        dispatch(
            "POST",
            "/v1/services/pets",
            Some(pets.post_body),
            reqwest::StatusCode::CREATED,
            record(&pets, 1),
            Some(by_id_path(&pets, 1)),
        ),
        dispatch(
            "GET",
            "/v1/services/pets/{id}",
            None,
            reqwest::StatusCode::OK,
            record(&pets, 1),
            None,
        ),
        dispatch(
            "PUT",
            "/v1/services/pets/{id}",
            Some(pets.put_body),
            reqwest::StatusCode::OK,
            pets.put_result,
            None,
        ),
        dispatch(
            "PATCH",
            "/v1/services/pets/{id}",
            Some(pets.patch_body),
            reqwest::StatusCode::OK,
            r#"{"id":1,"name":"Rexy","species":"dog","age":4}"#,
            None,
        ),
        dispatch(
            "DELETE",
            "/v1/services/pets/{id}",
            None,
            reqwest::StatusCode::NO_CONTENT,
            "",
            None,
        ),
    ]
}

#[tokio::test]
async fn every_listed_endpoint_is_the_method_the_socket_dispatches() {
    // The walk writes to the store, so it needs a data root of its own that starts empty (the
    // `[]` rows are only true for a store nothing has created into yet).
    let store = StoreFixture::new("routes-dispatch");
    let server = TestServer::start(store.state()).await;
    let client = client();

    let listing = json_body(
        client
            .get(server.url("/v1/services/routes"))
            .send()
            .await
            .expect("the listing is reachable"),
    )
    .await;
    let entries = listing_entries(&listing);

    // The plan has to BE the listing: same rows, same order, same advertised methods and paths.
    // Without this, the dispatch checks below could be proving some other table's contract.
    let plan = dispatch_plan();
    let listed: Vec<(&str, &str)> = entries
        .iter()
        .map(|&(method, path, _)| (method, path))
        .collect();
    let planned: Vec<(&str, &str)> = plan.iter().map(|row| (row.method, row.template)).collect();
    assert_eq!(
        listed, planned,
        "the listing and the dispatch plan are not the same rows in the same order"
    );

    // Send what the listing promises, then check the contract it promises back -
    // a 200 from a different handler is still a broken listing.
    for row in &plan {
        // The advertised verb is parsed from the advertised string, so an unusable method in
        // the listing is a loud failure here rather than a silently skipped row.
        let label = format!("{} {}", row.method, row.template);
        let method = reqwest::Method::from_bytes(row.method.as_bytes())
            .unwrap_or_else(|err| panic!("the listing advertises an unusable method: {err}"));
        let concrete = row
            .template
            .replace("{value}", PARAM_PROBE_VALUE)
            .replace("{id}", PARAM_PROBE_ID);
        let answer = send(&client, &server, method, &concrete, row.body).await;
        assert_eq!(
            answer.status, row.status,
            "{label} ({concrete}) is advertised by the listing but not dispatched"
        );
        assert_eq!(
            answer.body, row.answer,
            "{label} answered a different contract than the listing describes"
        );
        assert_eq!(
            answer.location.as_deref(),
            row.location.as_deref(),
            "{label} is not the row the listing promises a `location` header for"
        );
        if row.answer.is_empty() {
            // The `204` rows have no body to describe, so they must not name a media type.
            assert_eq!(
                answer.content_type, None,
                "{label} sent a body-less answer with a content type"
            );
        }
    }

    // The method is part of the promise, not just the path: the same path with a
    // method the listing does not advertise is a 405 (registered, wrong method),
    // never a 404 (not registered). Every listed path pattern is covered here, one from
    // each direction, and the `allow` header is the router's own account of the verbs that
    // ARE registered on it - which is what turns "not a 404" into a positive statement.
    // `/v1/services/echo/probe` is the listed `{value}` template with `PARAM_PROBE_VALUE`
    // substituted, i.e. the same concrete path used above.
    let pets_sample = pets();
    let locations_sample = locations();
    let pets_one = by_id_path(&pets_sample, 1);
    for (method, path, allow) in [
        (reqwest::Method::POST, "/v1/services/echo/probe", "GET,HEAD"),
        (reqwest::Method::DELETE, "/v1/services/health", "GET,HEAD"),
        (reqwest::Method::GET, "/v1/services/echo", "POST"),
        (
            reqwest::Method::POST,
            pets_one.as_str(),
            pets_sample.allow_by_id,
        ),
        (
            reqwest::Method::PUT,
            locations_sample.collection,
            locations_sample.allow_collection,
        ),
        (
            reqwest::Method::OPTIONS,
            pets_sample.collection,
            pets_sample.allow_collection,
        ),
    ] {
        // `Method` is not `Copy` and the request consumes it, so the assertion label is
        // rendered BEFORE the move; `{method}` afterwards would be a borrow-after-move.
        let label = format!("{method} {path}");
        let answer = send(&client, &server, method, path, None).await;
        assert_eq!(
            answer.status,
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
            "{label} must be refused as a method, which is what proves the listing's method is the registered one"
        );
        assert_eq!(
            answer.allow.as_deref(),
            Some(allow),
            "{label} says a different set of verbs is registered than the listing covers"
        );
        assert_eq!(answer.body, "", "{label} refused a method with a body");
    }

    // A `GET` route answers `HEAD` as well (which is why `HEAD` appears in every `allow` above):
    // the same status and headers, and no body - the one response shape the playground has to
    // survive without a body to render.
    let head = send(
        &client,
        &server,
        reqwest::Method::HEAD,
        pets_sample.collection,
        None,
    )
    .await;
    assert_eq!(head.status, reqwest::StatusCode::OK);
    assert_eq!(head.content_type.as_deref(), Some("application/json"));
    assert_eq!(head.body, "");

    server.shutdown().await;
}

#[tokio::test]
async fn the_builtin_surface_leaves_the_discovered_table_intact() {
    let server = TestServer::start(AppState::new(scratch_root("playground-priority"))).await;
    let client = client();

    // The builtins are registered in the same `app()` call as the generated table,
    // so the risk is shadowing. The exact health bytes and the exact prefixed echo
    // value are the proof that nothing in front of them changed.
    let health = client
        .get(server.url("/v1/services/health"))
        .send()
        .await
        .expect("health is still reachable");
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    assert_eq!(json_body(health).await, health_body());

    let echo = client
        .get(server.url("/v1/services/echo/hello?prefix=pre-"))
        .send()
        .await
        .expect("echo is still reachable");
    assert_eq!(json_body(echo).await, json!({ "value": "pre-hello" }));

    // Each builtin path is GET-only, so any other method is a 405 on a registered
    // path rather than a silent fallback to something else.
    for (method, path) in [
        (reqwest::Method::POST, "/"),
        (reqwest::Method::DELETE, "/"),
        (reqwest::Method::POST, "/v1/services/routes"),
        (reqwest::Method::PUT, "/playground.css"),
        (reqwest::Method::POST, "/playground.js"),
    ] {
        // Same borrow rule as above: render the label before `request` moves `method`.
        let label = format!("{method} {path}");
        let response = client
            .request(method, server.url(path))
            .send()
            .await
            .expect("the request reaches the server");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
            "{label} must be refused as a method, not treated as an unknown path"
        );
    }

    // No prefix route anywhere in the builtin surface: a longer path is simply not
    // registered (a `nest`/fallback would answer these and break the 404 contract).
    for path in [
        "/playground",
        "/playground.css.map",
        "/playground.js.map",
        "/v1/services/routes/extra",
        "/v1/services/routes.json",
    ] {
        let response = client
            .get(server.url(path))
            .send()
            .await
            .expect("the request reaches the server");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND,
            "{path} must not be answered by a builtin"
        );
    }

    server.shutdown().await;
}
