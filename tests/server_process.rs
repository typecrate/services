//! The executable AS A PROCESS: what the terminal of a running start actually shows.
//!
//! Every other test of this log surface asserts it from the inside. `src/logging.rs`'s own tests
//! drive a real `tracing` dispatch into a `Vec<u8>`, and `tests/logging.rs` serves the real router
//! on a real socket and reads the strings the terminal would show. Neither can prove the three
//! things only a process can prove, and all three are what the user's report is about:
//!
//! * the destination is **stdout** - `crate::logging::install` picks `io::stdout()`
//!   (`src/logging.rs:395`), and a test writing into its own process's stdout cannot see that;
//! * `install_logger()` runs BEFORE anything is served (`src/bin/server.rs:250`) - a subscriber
//!   installed after the first requests arrived would print nothing for them;
//! * `RUST_LOG` is read from the STARTED PROCESS's environment, and a knob it cannot read is
//!   named on **stderr** (`src/bin/server.rs:281-306`) rather than quietly changing verbosity.
//!
//! So each test below spawns `env!("CARGO_BIN_EXE_typecrate-services-server")` with piped
//! stdout/stderr, dials one request, kills the child (which closes the pipes, so the readers see
//! EOF and return everything the child wrote), and asserts whole lines.
//!
//! # Why the child binds `[::1]`, and why one child runs at a time
//!
//! `--port 0` makes the DEFAULT listener ephemeral, and the banner prints the port the OS
//! actually handed out - that is the port read back here, so this file never hard-codes 8000.
//! But the route table also DECLARES ports (today 8010 and 8800), and `serve` binds one listener
//! per declared port next to the default one - those two are not configurable. On the v4
//! loopback a child would therefore fight a developer's running `yarn start-rust` (which holds
//! `0.0.0.0:8010` and `0.0.0.0:8800`) and lose: Linux refuses a specific-address bind beside a
//! live wildcard bind on the same port, and the child exits 1 with "address already in use".
//! Binding the IPv6 loopback instead cannot collide with that v4 start at all, keeps every
//! listener loopback-only (nothing on the LAN is touched), and still reports an ephemeral port.
//!
//! Children are serialised by `CHILD_LOCK` for the same reason: two children of THIS binary
//! would collide with each other on those two declared ports. The guard is released only after
//! the child has been reaped, so the next one always binds a free port.
//!
//! # Why the probe path is `/v1/services/routes`
//!
//! It is a framework builtin of the DEFAULT listener (`src/server/mod.rs:491`), so it is mounted,
//! answers `200` and is logged whatever the route files declare - and it is itself proof that a
//! builtin is logged like any other matched route.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Child;

use typecrate_services::logging::LEVEL_ENV;
use typecrate_services::server::{
    MAX_REQUEST_BODY_BYTES, REQUEST_LOG_PREFIX, ROUTE_METADATA_PATH, normalize_lexically,
};

/// How long the child may take to print its first stdout line before it counts as hung. The
/// debug binary starts in well under a second; this only catches a real stall.
const BANNER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the two readers may take to reach EOF once the child has been killed.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// The bind every child is started with: the IPv6 loopback, port from the OS. The module doc
/// explains why this cannot collide with a running start; `--port 0` is what keeps the default
/// listener off 8000, and the port asserted below is the one the child REPORTS, never a guess.
const LOOPBACK: &str = "[::1]";

/// One child at a time - see the module doc (the declared ports 8010/8800 are fixed, so two live
/// children would fail to bind).
///
/// A `tokio` mutex rather than a `std` one because the guard is held across `.await` for the whole
/// child lifetime: `clippy::await_holding_lock` (in `all`, so `lint:strict` denies it) is right
/// that a std guard blocks the whole THREAD while another task awaits, and the tokio guard
/// instead parks the task, which is what the other tests of this binary want.
static CHILD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One child process and everything it produced: the port it bound, the exit of the probe
/// request, and every line it wrote to each stream (stdout without the banner's first line,
/// which is kept separately because that line is where the port comes from).
struct Process {
    listening: String,
    port: u16,
    /// The data root handed to `--database-root`, normalised exactly as the child normalises it,
    /// so the `database root:` banner line can be asserted byte for byte.
    database_root: PathBuf,
    stdout: Vec<String>,
    stderr: Vec<String>,
    status: u16,
}

/// Starts the built executable, serves exactly ONE request through it, reaps it, and returns
/// everything it printed.
///
/// `rust_log` is the child's `RUST_LOG`: `None` REMOVES the variable, so a developer's own shell
/// value cannot leak into the case that is testing the default; `Some(raw)` sets exactly those
/// bytes (`&OsStr` rather than `&str` because one case passes a value that is not valid text).
async fn serve_one(rust_log: Option<&OsStr>, marker: &str) -> Process {
    // Held for the whole body and released only after the child is reaped below. A tokio guard has
    // no poisoned state, so there is nothing to unwrap: a test that panics while holding it still
    // hands the next test a usable lock.
    let _slot = CHILD_LOCK.lock().await;

    let root = ScratchRoot::new(marker);
    let database_root = normalize_lexically(&root.path);

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_typecrate-services-server"));
    command
        .arg("--addr")
        .arg(LOOPBACK)
        .arg("--port")
        // "Any free port" for the default listener; the banner reports which one that was.
        .arg("0")
        // A writable scratch root, so the child never creates folders under the default
        // `<repo-parent>/temporary/dump` a developer may be using.
        .arg("--database-root")
        .arg(&database_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Belt and braces for a test that panics mid-body: the child never outlives the test.
        .kill_on_drop(true);

    match rust_log {
        Some(raw) => command.env(LEVEL_ENV, raw),
        None => command.env_remove(LEVEL_ENV),
    };

    let mut child: Child = command
        .spawn()
        .expect("the built executable spawns with piped streams");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    // Line 1 is read here because it carries the port; everything after it is drained in the
    // background while this task talks to the child, because the banner is longer than the lines
    // asserted below and a child whose pipe buffer fills blocks before it serves anything.
    let mut banner = BufReader::new(stdout).lines();
    let listening = banner_line(&mut banner).await;
    let port = announced_port(&listening);
    let stdout_task = tokio::spawn(read_to_eof(banner));
    let stderr_task = tokio::spawn(read_to_eof(BufReader::new(stderr).lines()));

    let status = probe(port, marker).await;

    // The request has been answered, so any request line it produced is ALREADY in the pipe -
    // `log_request` emits before `next.run`, i.e. before the response exists. Killing the child
    // (SIGKILL: no signal handler, so no extra stderr line) closes both pipes, the readers below
    // reach EOF, and what they returned is the complete output of that process.
    child.kill().await.expect("the child is reaped");

    let stdout = read_with_timeout(stdout_task, "stdout").await;
    let stderr = read_with_timeout(stderr_task, "stderr").await;

    // The scratch root goes only now: the child that writes under it is reaped above, so its
    // folders cannot be recreated behind the `remove_dir_all`. The `CHILD_LOCK` guard is released
    // with the rest of this scope, i.e. after the same kill, which is what lets the next child
    // bind the declared ports again.
    drop(root);

    Process {
        listening,
        port,
        database_root,
        stdout,
        stderr,
        status,
    }
}

/// The banner's first stdout line, with a deadline: a child that never prints is a failure, not
/// a hung suite.
async fn banner_line<R>(lines: &mut tokio::io::Lines<BufReader<R>>) -> String
where
    R: AsyncRead + Unpin,
{
    tokio::time::timeout(BANNER_TIMEOUT, lines.next_line())
        .await
        .expect("the first stdout line arrives")
        .expect("stdout is readable")
        .expect("stdout is valid UTF-8")
}

/// Every line until the pipe closes, in order. EOF arrives only once the child is gone, which is
/// why the caller puts a deadline on the whole read.
async fn read_to_eof<R>(mut lines: tokio::io::Lines<BufReader<R>>) -> Vec<String>
where
    R: AsyncRead + Unpin,
{
    let mut written = Vec::new();
    while let Some(line) = lines
        .next_line()
        .await
        .expect("the child's output is valid UTF-8")
    {
        written.push(line);
    }
    written
}

/// Joins a reader, with a deadline, so a child that refuses to die fails the test instead of the
/// whole suite.
async fn read_with_timeout(
    task: tokio::task::JoinHandle<Vec<String>>,
    stream: &str,
) -> Vec<String> {
    tokio::time::timeout(DRAIN_TIMEOUT, task)
        .await
        .unwrap_or_else(|_| panic!("the child closes {stream} when it is killed"))
        .unwrap_or_else(|_| panic!("the {stream} reader does not panic"))
}

/// The port the OS handed the child, READ out of the banner rather than assumed: `--port 0` means
/// "any free port", so hard-coding one here would both lie about the URL and risk a collision.
fn announced_port(listening: &str) -> u16 {
    let rest = listening
        .strip_prefix(&format!(
            "typecrate-services-server listening on {LOOPBACK}:"
        ))
        .unwrap_or_else(|| panic!("the banner names the loopback bind it made, got `{listening}`"));
    // `<port> - this interface only`: an explicit loopback bind (see `banner_lines`).
    let (port, _) = rest.split_once(' ').unwrap_or((rest, ""));
    port.parse()
        .unwrap_or_else(|err| panic!("the banner prints a numeric port, got `{port}`: {err}"))
}

/// One request, to the child's DEFAULT listener, over the ephemeral port it reported.
///
/// No pooled connections: the request is its own socket, so it cannot be served from a stale
/// keep-alive the child never saw.
async fn probe(port: u16, marker: &str) -> u16 {
    let client = reqwest::Client::builder()
        .pool_idle_timeout(Duration::ZERO)
        .build()
        .expect("a default client with no TLS backend builds for http://");
    client
        .get(format!(
            "http://{LOOPBACK}:{port}{ROUTE_METADATA_PATH}?marker={marker}"
        ))
        .send()
        .await
        .expect("the child answers the request")
        .status()
        .as_u16()
}

/// Every `Incoming request: ...` line the child printed, whole and in order.
fn request_lines(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .map(String::as_str)
        .filter(|line| line.starts_with(REQUEST_LOG_PREFIX))
        .collect()
}

/// The one line a listener prints for the probe request: the TypeScript gateway's format
/// (`packages/underload/service/src/server/start.ts:803`), carrying the authority the child was
/// dialed with and the query exactly as it arrived.
fn expected_line(port: u16, marker: &str) -> String {
    format!(
        "{REQUEST_LOG_PREFIX}[GET] http://{LOOPBACK}:{port}{ROUTE_METADATA_PATH}?marker={marker}"
    )
}

/// A private, writable data root that deletes itself, so no child touches the default dump root.
struct ScratchRoot {
    path: PathBuf,
}

impl ScratchRoot {
    fn new(name: &str) -> Self {
        // <repo>/packages/typecrate/services -> ../../../ == repository root, then the scratch
        // tree this package owns (root AGENTS.md: throwaway work goes in the repo-root `Temp/`).
        let path = normalize_lexically(&Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../../Temp/typecrate-services/server-process/{}-{name}",
            std::process::id()
        )));
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|err| panic!("could not create `{}`: {err}", path.display()));
        Self { path }
    }
}

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        // Best effort: disposable scratch state below the repository-root `Temp/`.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[tokio::test]
async fn the_executable_prints_the_request_line_on_its_own_stdout() {
    let process = serve_one(None, "process-default").await;

    // The banner line itself, exact, INCLUDING the ephemeral port: the child bound the loopback
    // and printed what it got on stdout.
    assert_eq!(
        process.listening,
        format!(
            "typecrate-services-server listening on {LOOPBACK}:{} - this interface only",
            process.port
        )
    );
    assert_ne!(
        process.port, 0,
        "the OS reports the port it actually handed out"
    );

    // The next two banner lines, byte for byte (padding included), pin both that the banner is on
    // stdout too and that `--database-root` reached the child. They are asserted by position
    // because they come before any route-dependent line.
    assert_eq!(
        process.stdout.first().expect("the banner continues"),
        &format!("  database root:    {}", process.database_root.display())
    );
    assert_eq!(
        process.stdout.get(1).expect("the banner continues"),
        &format!("  max request body: {MAX_REQUEST_BODY_BYTES} bytes")
    );

    assert_eq!(
        process.status, 200,
        "the builtin route listing on the default listener answered"
    );

    // The point of this file: ONE line, on the child's OWN stdout, byte for byte - which also
    // proves the subscriber was installed before serving started, because the line is an event
    // and an event with no subscriber in the process is dropped.
    assert_eq!(
        request_lines(&process.stdout),
        vec![expected_line(process.port, "process-default").as_str()],
        "exactly one request line on stdout, untagged and untimestamped"
    );

    // A default start has nothing to say on stderr, and the request line never goes there.
    assert_eq!(
        process.stderr,
        Vec::<String>::new(),
        "no knob was set and none was refused, so stderr stays clean"
    );
}

#[tokio::test]
async fn the_executable_drops_the_request_line_at_warn() {
    let process = serve_one(Some(OsStr::new("warn")), "process-warn").await;

    // The request really was served: the line is gone because of the filter, not because the
    // child never answered.
    assert_eq!(
        process.status, 200,
        "a quiet start still serves the request"
    );
    assert_eq!(
        request_lines(&process.stdout),
        Vec::<&str>::new(),
        "`{LEVEL_ENV}=warn` hides the per-request line - it is an INFO-severity fact, so a \
         tightened knob is the quiet start an operator means by it"
    );

    // The banner survives ANY `RUST_LOG`, because it is `println!` rather than a `tracing` event:
    // a start with no request lines is not a start with no output.
    assert!(
        process.stdout.contains(&format!(
            "  max request body: {MAX_REQUEST_BODY_BYTES} bytes"
        )),
        "the banner is not filtered: {:#?}",
        process.stdout
    );
}

#[tokio::test]
async fn the_executable_keeps_info_and_drops_only_the_request_line() {
    // A comma-separated spec with a target-scoped item: the grammar the README and `--help`
    // promise, proven on a real process rather than on the parser alone.
    let process = serve_one(
        Some(OsStr::new("info,typecrate_services::request=off")),
        "process-target-off",
    )
    .await;

    assert_eq!(process.status, 200, "the request was served");
    assert_eq!(
        request_lines(&process.stdout),
        Vec::<&str>::new(),
        "the scoped `off` silences that ONE target while the rest of `info` stays on"
    );
    assert_eq!(
        process.stderr,
        Vec::<String>::new(),
        "a comma list the parser reads needs no notice on stderr"
    );
}

#[tokio::test]
async fn the_executable_names_an_unreadable_directive_on_stderr() {
    // The sibling diagnostics of this binary (the bind refusal, the port collision, the bad
    // address) are pinned byte for byte, so this one is too: a typo in a log filter has to be
    // visible, and it has to cost nothing else.
    let process = serve_one(Some(OsStr::new("bogus,info")), "process-bogus").await;

    assert_eq!(
        process.stderr,
        vec![
            "typecrate-services-server: ignoring the unreadable RUST_LOG directive \"bogus\" \
             (expected a level, or target=level)"
                .to_owned()
        ]
    );
    assert_eq!(process.status, 200, "a bad item never stops the start");
    assert_eq!(
        request_lines(&process.stdout),
        vec![expected_line(process.port, "process-bogus").as_str()],
        "the readable half of the spec still applies: `bogus` is dropped, `info` is honoured"
    );
}

/// A `RUST_LOG` the OS can hold and `std::env::var` cannot read: `Err(NotUnicode)`.
///
/// `std::env::var_os` is what lets `install_logger` see that the knob WAS set and report it,
/// instead of reading the bad value as "unset" and quietly running at the default with nothing on
/// stderr - the one outcome the contract in `src/bin/server.rs:270-273` rules out. Unix-only,
/// because a windows `OsString` is UTF-16 and cannot carry a lone `0xFF` byte.
#[cfg(unix)]
#[tokio::test]
async fn the_executable_names_a_value_that_is_not_valid_text() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let raw = OsString::from_vec(b"info\xFF".to_vec());
    let process = serve_one(Some(raw.as_os_str()), "process-non-unicode").await;

    assert_eq!(
        process.stderr,
        vec![
            "typecrate-services-server: ignoring the unreadable RUST_LOG value - it is not valid \
             text (expected a level, or target=level)"
                .to_owned()
        ],
        "an undecodable knob is reported like an undecodable directive"
    );
    assert_eq!(process.status, 200, "and the start proceeds at the default");
    assert_eq!(
        request_lines(&process.stdout),
        vec![expected_line(process.port, "process-non-unicode").as_str()],
        "the default `info` filter runs, so the request line is printed"
    );
}
