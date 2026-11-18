//! The process's log surface: one hand-written [`tracing::Subscriber`] plus the `RUST_LOG`
//! level filter, and the target the gateway's per-request line is emitted on.
//!
//! Why this file exists at all: `typecrate-provider` (compiled into this binary, served on
//! 8800) already logs through the `tracing` facade - the per-completion token line
//! (`provider/src/serve.rs:617-629`), the `[failover]` lines (`provider/src/client.rs:865-879`),
//! the `[retry] HTTP 429` line whose stated intent is "so a pause is not invisible in the
//! terminal" (`provider/src/transport.rs:1415-1418`), the vault-down `warn!`
//! (`provider/src/serve.rs:553`), the `error!`s (`:691`, `:775`, `:1394`) and the load `trace!`
//! (`provider/src/vault.rs:916`). A `tracing` event with **no subscriber installed is dropped**:
//! `tracing`'s default dispatch is `NoSubscriber`, which answers `enabled(..)` with `false`.
//! That is why `yarn start-rust` printed the banner and then nothing: every one of those lines
//! was being thrown away. Installing this subscriber is what makes them visible.
//!
//! Why hand-written rather than `tracing-subscriber`: **no new package may enter this lockfile**.
//! This standalone workspace has no vendored registry (`provider/knowledge.md:74-85`), the host
//! cannot reach `index.crates.io`, and `tracing-subscriber` / `matchers` / `nu-ansi-term` /
//! `sharded-slab` / `env_logger` are all absent from `Cargo.lock` (they exist only in the
//! unrelated `services/nexus/Cargo.lock`). `tracing` 0.1.44 and `tracing-core` 0.1.36 are
//! already in it, so the subscriber below is written against those two crates only. The
//! provider's own lock-subset test (`provider/tests/transport.rs:2749-2795`) requires the
//! provider package set to stay a subset of this one, which is why the new edges are declared
//! here and never there.
//!
//! What one event becomes, exactly (one whole line, terminated by `\n`):
//!
//! * an event on [`REQUEST_TARGET`] - the gateway's per-request line - is the event's message
//!   **verbatim**, untagged and untimestamped, because that is the line the TypeScript gateway
//!   `console.log`s (`packages/underload/service/src/server/start.ts:803`) and the byte-for-byte
//!   parity of it is the contract;
//! * every other event is `<LEVEL padded to 5> <target>: <message>[ <field>=<value>...]`,
//!   e.g. ` WARN typecrate_provider::serve: provider route refused: ...`. Levels are spelled
//!   the way `tracing` spells them and are fixed-width so the targets line up; there is no
//!   timestamp, matching the untimestamped `console.log` lines of the TS gateway, and it keeps
//!   every rendered line a byte-exact string a test can pin.
//!
//! Two more properties worth naming, because they are deliberate:
//!
//! * a write error is **ignored** (a closed pipe, an EPIPE on stdout). `println!` panics on one,
//!   and `profile.release` sets `panic = "abort"` (`Cargo.toml:129-134`), so a logging failure
//!   would otherwise take the process down;
//! * spans are accepted and dropped. Nothing in this process opens one: there is no
//!   `tracing::span!` call anywhere in it (that macro is part of `tracing` itself and needs no
//!   feature to exist), and there is no `#[instrument]` either (that one IS gated - it comes from
//!   `tracing`'s `attributes` feature, which this crate turns off in `Cargo.toml`). A constant id
//!   with no store is therefore the honest answer, and it is why
//!   `new_span`/`record`/`enter`/`exit` below do nothing.

use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use tracing::level_filters::LevelFilter;
use tracing_core::field::{Field, Visit};
use tracing_core::span::{self, Id};
use tracing_core::subscriber::Subscriber;
use tracing_core::{Event, Level, Metadata};

/// The [`tracing`] target of the gateway's per-request line
/// (`src/server/request_log.rs`). It is public because its rendering rule (message verbatim) and
/// its exemption from the level filter both live here, while the line itself is emitted there.
pub const REQUEST_TARGET: &str = "typecrate_services::request";

/// The environment variable that controls verbosity. `RUST_LOG` is the name the only other Rust
/// server in this repo honours (`services/nexus/crates/nexus-server/src/main.rs:44-53`), so
/// there is one spelling of this knob across the repo rather than two.
pub const LEVEL_ENV: &str = "RUST_LOG";

/// The level an unset (or empty) `RUST_LOG` runs at: `info`, i.e. the token line, `[failover]`,
/// `[retry]`, the vault-down `warn!` and every `error!` are on, while the provider's `debug!`
/// request-body diagnostics and its `trace!` vault line stay off.
pub const DEFAULT_LEVEL: LevelFilter = LevelFilter::INFO;

/// The number every span id answers with. There is no span store (see the module doc), and
/// `span::Id` is a non-zero `u64`, so `0` is not available; `Id::from_u64` is not a `const fn`,
/// which is why this is the raw number rather than an `Id`.
const NO_SPAN_ID: u64 = 1;

/// One `RUST_LOG` directive: a bare level (`debug`), or a target-scoped one
/// (`typecrate_provider=trace`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directive {
    /// The target prefix this directive narrows, `None` for the process-wide level.
    pub prefix: Option<String>,
    /// The maximum level the directive allows.
    pub level: LevelFilter,
}

/// The resolved level filter. Immutable for the life of the process, which is what lets the
/// `tracing` callsite registry cache interest for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    global: LevelFilter,
    scoped: Vec<Directive>,
    /// Directives the parser could not read, kept verbatim so the caller can say so out loud
    /// instead of silently logging at the default.
    ignored: Vec<String>,
}

impl Default for Filter {
    fn default() -> Self {
        Self::at(DEFAULT_LEVEL)
    }
}

impl Filter {
    /// A process-wide level with no target scoping.
    #[must_use]
    pub fn at(level: LevelFilter) -> Self {
        Self {
            global: level,
            scoped: Vec::new(),
            ignored: Vec::new(),
        }
    }

    /// The process-wide level every other event is measured against.
    #[must_use]
    pub fn global(&self) -> LevelFilter {
        self.global
    }

    /// The target-scoped directives, in the order they were written.
    #[must_use]
    pub fn scoped(&self) -> &[Directive] {
        &self.scoped
    }

    /// Directive text this parser could not understand, kept verbatim (see [`Filter::parse`]).
    #[must_use]
    pub fn ignored(&self) -> &[String] {
        &self.ignored
    }

    /// Whether an event from `target` at `level` is rendered.
    ///
    /// Rules, in order:
    ///
    /// 1. a target-scoped directive whose prefix matches wins over everything else, and the
    ///    **longest** matching prefix wins, so `typecrate_provider=info` still applies under
    ///    `RUST_LOG=trace,typecrate_provider::serve=warn`;
    /// 2. otherwise an event on [`REQUEST_TARGET`] is judged at **info** severity regardless of
    ///    the level it was recorded with: the line is a fixed fact about a request, not a
    ///    verbosity hint, so the knob decides it exactly like an `info` event - visible at
    ///    `info`/`debug`/`trace`, hidden at `warn`/`error`/`off` (which is what a `-q` start
    ///    means). `RUST_LOG=typecrate_services::request=off` silences that one line while the
    ///    rest of `info` keeps printing;
    /// 3. otherwise the event must be at or below the process-wide level.
    ///
    /// `Level` and `LevelFilter` compare the way `tracing` means it: a filter enables the
    /// levels at or below itself, and `LevelFilter::OFF` is above every level, so it enables
    /// nothing (`tracing-core/metadata.rs:952-1053` is where that ordering lives).
    #[must_use]
    pub fn allows(&self, target: &str, level: Level) -> bool {
        match self
            .scoped
            .iter()
            .filter(|directive| {
                directive
                    .prefix
                    .as_deref()
                    .is_some_and(|prefix| target.starts_with(prefix))
            })
            .max_by_key(|directive| directive.prefix.as_ref().map_or(0, |prefix| prefix.len()))
        {
            Some(directive) => level <= directive.level,
            None if target == REQUEST_TARGET => Level::INFO <= self.global,
            None => level <= self.global,
        }
    }

    /// The minimal `RUST_LOG` grammar, spelled out in `HELP` (the `ENVIRONMENT:` section,
    /// pinned by `help_documents_every_line_a_start_prints`) and in the README's "What a
    /// running server prints" section, so these four rules are stated in three places:
    ///
    /// * comma- or space-free items split on `,`; surrounding whitespace and empty items
    ///   (`info,,debug`) are ignored, so a shell-built list with a stray comma still works;
    /// * `<level>` sets the process-wide level; the last bare level in the spec wins;
    /// * `<target-prefix>=<level>` caps the events whose `target` **starts with** that prefix
    ///   (module paths, so `typecrate_provider` covers `typecrate_provider::serve`), and an
    ///   EMPTY prefix (`=debug`) is the process-wide level again rather than a prefix that
    ///   matches every target;
    /// * levels are case-insensitive: `off`/`none`, `error`, `warn`, `info`, `debug`, `trace`;
    /// * an item that is not one of those shapes is collected in [`Filter::ignored`] rather
    ///   than guessed at or aborted on - a malformed knob must never stop the process, and
    ///   never panic under `panic = "abort"`. `src/bin/server.rs` names each one on stderr.
    ///
    /// An unset or empty spec is [`DEFAULT_LEVEL`].
    #[must_use]
    pub fn parse(spec: &str) -> Self {
        let mut filter = Self::default();
        for item in spec.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            // `=debug` (an empty prefix) is the global level again - the prefix part only means
            // "narrow this target" when there is a prefix.
            let (prefix, level_text) = match item.split_once('=') {
                Some((prefix, level)) => (Some(prefix.trim()), level.trim()),
                None => (None, item),
            };
            let Some(level) = parse_level(level_text) else {
                filter.ignored.push(item.to_owned());
                continue;
            };
            match prefix.filter(|prefix| !prefix.is_empty()) {
                Some(prefix) => filter.scoped.push(Directive {
                    prefix: Some(prefix.to_owned()),
                    level,
                }),
                None => filter.global = level,
            }
        }
        filter
    }
}

/// One level word -> [`LevelFilter`], case-insensitively. `none` is accepted as a synonym for
/// `off` because that is what the `env_logger`/`tracing-subscriber` documentation calls it.
fn parse_level(text: &str) -> Option<LevelFilter> {
    match text.to_ascii_lowercase().as_str() {
        "off" | "none" => Some(LevelFilter::OFF),
        "error" => Some(LevelFilter::ERROR),
        "warn" => Some(LevelFilter::WARN),
        "info" => Some(LevelFilter::INFO),
        "debug" => Some(LevelFilter::DEBUG),
        "trace" => Some(LevelFilter::TRACE),
        _ => None,
    }
}

/// The terminal subscriber: one whole line per event, into anything that is a [`io::Write`]
/// (`std::io::stdout()` in the executable; a `Vec<u8>` in a test, which is what makes the exact
/// bytes of every line assertable; and a sink that always answers `Err`, which is what pins the
/// "a write failure is dropped" half of the contract).
///
/// The sink is shared behind `Arc<Mutex<..>>` so a reader can inspect what was written after the
/// subscriber has been moved into the global `Dispatch`.
pub struct LogSubscriber<W> {
    sink: Arc<Mutex<W>>,
    filter: Filter,
}

impl<W> LogSubscriber<W> {
    /// A subscriber over an already-shared sink (see the shared handle, [`LogSubscriber::sink`]).
    #[must_use]
    pub fn shared(sink: Arc<Mutex<W>>, filter: Filter) -> Self {
        Self { sink, filter }
    }

    /// A subscriber over its own sink.
    #[must_use]
    pub fn new(sink: W, filter: Filter) -> Self {
        Self::shared(Arc::new(Mutex::new(sink)), filter)
    }

    /// The filter this subscriber applies.
    #[must_use]
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// A handle on the sink, so a test (or an embedder) can read the rendered bytes.
    #[must_use]
    pub fn sink(&self) -> Arc<Mutex<W>> {
        Arc::clone(&self.sink)
    }
}

impl<W: io::Write> LogSubscriber<W> {
    /// Renders one event as exactly one line (including its trailing `\n`).
    ///
    /// Split out from the `Subscriber` impl so the rendering rule - the whole contract of this
    /// file - is unit-testable without a `Context`.
    #[must_use]
    pub fn render(event: &Event<'_>) -> String {
        let metadata = event.metadata();
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message;
        let target = metadata.target();

        // The access-log line is its message and nothing else: no level, no target, no
        // timestamp - `start.ts:803` prints `Incoming request: [GET] http://host/path` and the
        // byte-for-byte shape of that line is the parity contract. A message-less event on this
        // target falls through to the ordinary rendering rather than printing a blank line.
        if target == REQUEST_TARGET
            && let Some(message) = message.as_deref()
        {
            let mut line = message.to_owned();
            line.push('\n');
            return line;
        }

        // `{:<5}` is the alignment: `INFO `/`WARN ` are four characters wide, `ERROR`/`DEBUG`/
        // `TRACE` are five, so every target starts in the same column.
        let mut line = format!("{:<5} {target}: ", metadata.level());
        // The message first, then any named field the event carried (`vendors=3`), space
        // separated. `message` is absent only for an event with named fields and no message, in
        // which case the fields follow the target directly.
        let mut first = true;
        for part in message
            .as_deref()
            .into_iter()
            .chain(visitor.fields.iter().map(String::as_str))
        {
            if !first {
                line.push(' ');
            }
            first = false;
            line.push_str(part);
        }
        line.push('\n');
        line
    }

    /// One write of one whole line, errors discarded on purpose: a closed stdout must not be
    /// fatal in a process built with `panic = "abort"` (see the module doc).
    fn write_line(&self, line: &str) {
        // A poisoned mutex still holds a perfectly usable sink, and a log line is not a
        // reasonable place to panic - hence `unwrap_or_else(into_inner)` rather than `expect`.
        //
        // The lock is deliberately held ACROSS the write (not just around a buffer swap): that
        // is what makes one event one contiguous line, since two tasks logging at the same moment
        // would otherwise interleave half a line each into the same sink. It costs a held lock
        // while stdout flushes, which is the right trade here - the alternative is a lock-free
        // byte stream with torn lines, and a torn request line breaks the byte-exact parity that
        // is the whole point of this file.
        let mut sink = self
            .sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = sink.write_all(line.as_bytes());
    }
}

impl<W: io::Write + Send + 'static> Subscriber for LogSubscriber<W> {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.filter.allows(metadata.target(), *metadata.level())
    }

    // Spans are answered and dropped - nothing in this process opens one (module doc). The id is
    // a CONSTANT, so if a dependency ever does open spans they all ALIAS one id: `enter`/`exit`
    // would be untraceable and `Span::current()` would name the wrong span. That is acceptable
    // only because no span exists here today; a future `#[instrument]` (or a dependency that
    // depends on span context) needs a real store, not a patched constant.
    fn new_span(&self, _span: &span::Attributes<'_>) -> Id {
        Id::from_u64(NO_SPAN_ID)
    }

    fn record(&self, _span: &Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        self.write_line(&Self::render(event));
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// Pulls the `message` field out of an event and renders every other field as `name=value`.
///
/// Only `record_debug` and `record_str` are implemented because everything else in
/// [`tracing_core::field`] forwards to `record_debug` by default, and:
///
/// * a format-args message (`info!("{} {n}", value)`) arrives as `fmt::Arguments`, whose `Debug`
///   is its `Display` - so `{:?}` produces the formatted text, unquoted;
/// * a `str`/`String` value takes the `record_str` path instead of `{:?}`, which is what keeps
///   `label=name` from printing as `label="name"`.
#[derive(Default)]
struct LineVisitor {
    message: Option<String>,
    /// Rendered `name=value` pairs, in the order the event carried them.
    fields: Vec<String>,
}

impl LineVisitor {
    fn push(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = Some(value);
        } else {
            self.fields.push(format!("{name}={value}"));
        }
    }
}

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field.name(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.push(field.name(), format!("{value:?}"));
    }
}

/// Installs [`LogSubscriber`] on stdout for the whole process, at the level `RUST_LOG` asks for.
///
/// `spec` is the raw value (already read by the caller, so the parse stays a pure function -
/// `std::env::set_var` is `unsafe` in edition 2024 and `set_global_default` is process-global, so
/// neither can be exercised by a parallel test). `None` is "unset" and means [`DEFAULT_LEVEL`].
///
/// The live [`Filter`] comes back so the caller can report [`Filter::ignored`] on stderr instead
/// of letting a typo in the knob look like working logging. An `Err` means a subscriber was
/// already installed (only possible for an embedder that installed its own before calling this);
/// the process keeps running either way.
pub fn install(spec: Option<&str>) -> Result<Filter, String> {
    let filter = Filter::parse(spec.unwrap_or(""));
    let subscriber = LogSubscriber::new(io::stdout(), filter.clone());
    match tracing::subscriber::set_global_default(subscriber) {
        Ok(()) => Ok(filter),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Every level word the grammar accepts, in the order `parse_level` spells them.
    #[test]
    fn a_bare_level_sets_the_global_filter_and_nothing_else() {
        let level = |text: &str| Filter::parse(text).global();
        assert_eq!(level("off"), LevelFilter::OFF);
        assert_eq!(level("none"), LevelFilter::OFF);
        assert_eq!(level("error"), LevelFilter::ERROR);
        assert_eq!(level("warn"), LevelFilter::WARN);
        assert_eq!(level("info"), LevelFilter::INFO);
        assert_eq!(level("debug"), LevelFilter::DEBUG);
        assert_eq!(level("trace"), LevelFilter::TRACE);
        // Case is irrelevant, and so is padding around the item.
        assert_eq!(level("  WARN  "), LevelFilter::WARN);
        assert_eq!(level("Error"), LevelFilter::ERROR);

        // Unset (empty), empty items and a spec of only commas all stay at the default.
        assert_eq!(Filter::parse("").global(), DEFAULT_LEVEL);
        assert_eq!(Filter::parse("").scoped(), []);
        assert_eq!(Filter::parse(", ,").global(), DEFAULT_LEVEL);
        // The last bare level in the spec wins.
        assert_eq!(
            Filter::parse("info,trace,debug").global(),
            LevelFilter::DEBUG
        );
    }

    #[test]
    fn a_target_prefixed_directive_scopes_its_level() {
        let filter = Filter::parse("info,typecrate_provider=trace");
        assert_eq!(filter.global(), LevelFilter::INFO);
        assert_eq!(
            filter.scoped(),
            [Directive {
                prefix: Some("typecrate_provider".to_owned()),
                level: LevelFilter::TRACE,
            }]
        );
        // An empty prefix is the global level, not a prefix that matches everything.
        assert_eq!(Filter::parse("=debug").global(), LevelFilter::DEBUG);
        assert_eq!(Filter::parse("=debug").scoped(), []);
    }

    /// A knob that is misconfigured must say so and keep serving, never guess and never abort.
    #[test]
    fn an_unreadable_directive_is_reported_not_guessed() {
        let filter = Filter::parse("bogus,typecrate_provider=nope,debug");
        assert_eq!(filter.global(), LevelFilter::DEBUG);
        assert_eq!(filter.scoped(), []);
        assert_eq!(
            filter.ignored(),
            ["bogus".to_owned(), "typecrate_provider=nope".to_owned()]
        );
    }

    /// The full decision matrix of [`Filter::allows`]: global ceiling, target scoping, longest
    /// prefix, and the [`REQUEST_TARGET`] exemption.
    #[test]
    fn the_filter_allows_by_target_then_by_level() {
        let info = Filter::at(LevelFilter::INFO);
        assert!(info.allows("typecrate_provider::serve", Level::INFO));
        assert!(info.allows("typecrate_provider::serve", Level::WARN));
        assert!(!info.allows("typecrate_provider::serve", Level::DEBUG));
        // The access-log target is judged at INFO severity whatever level its event carries:
        // that is the severity the line is a fact at, and it is what makes `RUST_LOG=warn`
        // behave like `-q` (the gateway emits it at INFO today).
        assert!(info.allows(REQUEST_TARGET, Level::INFO));
        assert!(info.allows(REQUEST_TARGET, Level::DEBUG));

        // A filter tighter than INFO hides the per-request line - the knob works on it.
        // The target is judged at INFO whatever level its event carries, so even an ERROR
        // event on it is hidden under `RUST_LOG=error`: the line is the request fact, and
        // that fact is an info-severity fact.
        let error_only = Filter::at(LevelFilter::ERROR);
        assert!(!error_only.allows(REQUEST_TARGET, Level::INFO));
        assert!(!error_only.allows(REQUEST_TARGET, Level::ERROR));
        // ... and an operator can scope it by target alone, because a matching directive is
        // checked before the global ceiling.
        let scoped_request = Filter::parse("info,typecrate_services::request=off");
        assert!(!scoped_request.allows(REQUEST_TARGET, Level::INFO));
        assert!(scoped_request.allows("typecrate_services::server", Level::INFO));

        // Scoping: the prefix is a module-path prefix, and the LONGEST match wins.
        let scoped = Filter::parse("trace,typecrate_provider=warn,typecrate_provider::serve=error");
        assert!(scoped.allows("typecrate_services::server", Level::TRACE));
        assert!(scoped.allows("typecrate_provider::client", Level::WARN));
        assert!(!scoped.allows("typecrate_provider::client", Level::INFO));
        assert!(scoped.allows("typecrate_provider::serve", Level::ERROR));
        assert!(!scoped.allows("typecrate_provider::serve", Level::WARN));
        // `off` is above every level, so it enables nothing - not even the access log.
        assert!(!Filter::at(LevelFilter::OFF).allows("anything", Level::ERROR));
        assert!(!Filter::at(LevelFilter::OFF).allows(REQUEST_TARGET, Level::ERROR));
    }

    /// A subscriber over an in-memory sink, driven through a real `tracing` dispatch.
    ///
    /// These tests share one process-wide callsite registry, and `tracing` CACHES its interest
    /// in a callsite per subscriber (`Subscriber::register_callsite` ->
    /// `Interest::always`/`sometimes`/`never`), re-evaluated only when a dispatch is created or
    /// dropped or when `rebuild_interest_cache` runs. Two captures running in parallel therefore
    /// stomp on each other's cached interest, and a callsite another test's filter happened to
    /// mark `never` first stays switched off for the whole process -- a line goes missing with
    /// nothing wrong in the subscriber. `CAPTURE_LOCK` serialises exactly that: both captures hold
    /// it for their whole body (guards are declared so the subscriber guard drops BEFORE the
    /// lock), so while it is held the only live dispatch is the one the test installed itself.
    static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

    /// An event on the access-log target is its message verbatim; every other event gets the
    /// level + target prefix; three events produce three `\n`-terminated lines.
    #[test]
    fn one_event_becomes_exactly_one_line() {
        let _serialise = CAPTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber = LogSubscriber::shared(Arc::clone(&sink), Filter::at(LevelFilter::TRACE));
        let _guard = tracing::subscriber::set_default(subscriber);
        // Interest is cached per (callsite, subscriber); rebuilding here is what makes the
        // callsites below be evaluated against THIS subscriber rather than whichever filter
        // touched them first.
        tracing::callsite::rebuild_interest_cache();

        tracing::info!(
            target: "typecrate_provider::serve",
            "[retry] HTTP 429 rate limited \u{2014} waiting {}ms before retrying",
            3000
        );
        tracing::info!(
            target: REQUEST_TARGET,
            "{}",
            "Incoming request: [GET] http://127.0.0.1:8000/v1/services/health"
        );
        tracing::trace!(
            target: "typecrate_provider::vault",
            vendors = 3,
            slots = 1,
            entries = 2,
            "vault artifact accepted"
        );

        let written = String::from_utf8(sink.lock().unwrap().clone()).expect("utf-8 output");
        assert_eq!(
            written,
            concat!(
                "INFO  typecrate_provider::serve: [retry] HTTP 429 rate limited \u{2014} waiting 3000ms before retrying\n",
                "Incoming request: [GET] http://127.0.0.1:8000/v1/services/health\n",
                "TRACE typecrate_provider::vault: vault artifact accepted vendors=3 slots=1 entries=2\n",
            )
        );
        // The line count is part of "one whole line per event", and no line may be empty.
        assert_eq!(written.matches('\n').count(), 3);
        assert!(!written.contains("\n\n"));
    }

    /// The level knob, over real dispatches: a `debug!` under `DEBUG` renders exactly one line,
    /// and a `trace!` under the same `DEBUG` filter never reaches the sink at all.
    ///
    /// Each probe is its own function because interest is cached PER CALLSITE: one callsite
    /// cannot be used for both halves, since whichever filter sees it first decides it for the
    /// life of the process.
    #[test]
    fn the_level_knob_decides_what_reaches_the_sink() {
        let _serialise = CAPTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let debug_sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let debug = LogSubscriber::shared(Arc::clone(&debug_sink), Filter::at(LevelFilter::DEBUG));
        let _debug_guard = tracing::subscriber::set_default(debug);
        tracing::callsite::rebuild_interest_cache();
        probe_debug();
        probe_trace();
        assert_eq!(
            String::from_utf8(debug_sink.lock().unwrap().clone()).expect("utf-8 output"),
            "DEBUG probe: debug line\n",
            "the debug line renders and the trace line does not"
        );
        drop(_debug_guard);

        let trace_sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let trace = LogSubscriber::shared(Arc::clone(&trace_sink), Filter::at(LevelFilter::TRACE));
        let _trace_guard = tracing::subscriber::set_default(trace);
        tracing::callsite::rebuild_interest_cache();
        probe_trace();
        assert_eq!(
            String::from_utf8(trace_sink.lock().unwrap().clone()).expect("utf-8 output"),
            "TRACE probe: trace line\n",
            "raising the filter to TRACE surfaces the line INFO and DEBUG both dropped"
        );
        drop(_trace_guard);

        // `RUST_LOG=warn` on the real dispatch path: the per-request line is judged at INFO
        // severity (see `Filter::allows`), so a tightened knob hides it - which is what an
        // operator means by "errors and warnings only" - while a provider warning still prints.
        let warn_sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let warn = LogSubscriber::shared(Arc::clone(&warn_sink), Filter::at(LevelFilter::WARN));
        let _warn_guard = tracing::subscriber::set_default(warn);
        tracing::callsite::rebuild_interest_cache();
        probe_request_at_info();
        probe_provider_warning();
        assert_eq!(
            String::from_utf8(warn_sink.lock().unwrap().clone()).expect("utf-8 output"),
            "WARN  typecrate_provider::serve: [retry] HTTP 429 rate limited \u{2014} waiting 3000ms before retrying\n",
            "`{LEVEL_ENV}=warn` drops the request line and keeps the provider warning"
        );
    }

    /// A sink that refuses every write, i.e. what a closed stdout looks like to the subscriber.
    ///
    /// Every other test here writes into an infallible `Vec<u8>`, so the "a write failure is
    /// dropped" half of the contract (the `let _ =` in `LogSubscriber::write_line`) could never
    /// fail without something that CAN fail. `attempts` is shared with the test so it can prove
    /// the event actually reached the sink, rather than only that the test body returned.
    struct FailingSink {
        attempts: Arc<AtomicUsize>,
    }

    impl FailingSink {
        /// Counts the attempt and produces the failure: the one body both write paths share, so
        /// neither can be reached without the counter moving.
        fn refuse(&self) -> io::Error {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            io::Error::new(io::ErrorKind::BrokenPipe, "stdout is closed")
        }
    }

    impl io::Write for FailingSink {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(self.refuse())
        }

        // `write_all` is what `write_line` calls. Overriding it instead of leaving the default
        // (which loops over `write`) is what keeps `attempts` at exactly one per event.
        fn write_all(&mut self, _buf: &[u8]) -> io::Result<()> {
            Err(self.refuse())
        }

        // The subscriber never flushes explicitly (stdout flushes itself per line), and a log
        // sink that reports "flushed" is the honest counterpart of a closed pipe: the failure
        // belongs to the write, not to the attempt to drain it.
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The operator-facing promise (`HELP`'s `WHAT IT PRINTS`, the README, `package.json`): a log
    /// line can never stop the server. `profile.release` sets `panic = "abort"` (`Cargo.toml`),
    /// so a `println!`-style panic on a closed stdout would abort a process that is otherwise
    /// serving fine - and a start whose stdout is closed (`nohup ... > /dev/null`, a supervisor
    /// that pipes it away and goes away itself) is routine, not exotic.
    #[test]
    fn a_write_failure_is_dropped_instead_of_taking_the_process_down() {
        let _serialise = CAPTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let attempts = Arc::new(AtomicUsize::new(0));
        let subscriber = LogSubscriber::new(
            FailingSink {
                attempts: Arc::clone(&attempts),
            },
            Filter::at(LevelFilter::TRACE),
        );
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();

        // `event` -> `write_line` -> a sink that answers `Err`. The fact that this call returns
        // is the first half of the assertion: the `Err` did not escape as a panic, which in the
        // release profile would have been an abort rather than a test failure.
        probe_failing_sink();

        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "the event reached the sink exactly once and its error was swallowed, not retried \
             and not raised"
        );
    }

    #[inline(never)]
    fn probe_failing_sink() {
        tracing::info!(target: "probe", "a line the sink refuses");
    }

    #[inline(never)]
    fn probe_request_at_info() {
        tracing::info!(
            target: REQUEST_TARGET,
            "{}",
            "Incoming request: [GET] http://127.0.0.1:8000/v1/services/health"
        );
    }

    #[inline(never)]
    fn probe_provider_warning() {
        tracing::warn!(
            target: "typecrate_provider::serve",
            "[retry] HTTP 429 rate limited \u{2014} waiting {}ms before retrying",
            3000
        );
    }

    #[inline(never)]
    fn probe_debug() {
        tracing::debug!(target: "probe", "debug line");
    }

    #[inline(never)]
    fn probe_trace() {
        tracing::trace!(target: "probe", "trace line");
    }
}
