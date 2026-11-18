//! `typecrate_services` - Rust-native core behind the `@typecrate/services` npm package.
//!
//! Native-only: host builds link this rlib crate-to-crate, and the package's own
//! executable links it to serve HTTP - no wasm, no serialisation, no JavaScript in
//! the loop. There is one consumer and one path (see `Cargo.toml` for why this is a
//! standalone workspace sitting in the npm package directory).
//!
//! The crate includes the native HTTP runtime: `server` (shared state + the axum
//! `Router` factory) and the `typecrate-services-server` executable in
//! `src/bin/server.rs`.
//!
//! CURRENT STATE: identity-level core (`CRATE_NAME`, `version`, `echo`) plus the
//! native server runtime below.

/// Crate name as Rust code refers to it (`-` becomes `_`, rust/README.md naming rule).
pub const CRATE_NAME: &str = "typecrate_services";

/// Single source of truth for the version is this crate's manifest; the npm
/// `package.json` line is asserted to match it by `version_matches_the_npm_line`.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Identity helper: returns its input unchanged.
///
/// It exists because it is the smallest function that exercises a real argument
/// and a real return value across the public API. Any behaviour other than "the
/// same bytes came back" is a bug, not in this function.
#[must_use]
pub fn echo(text: &str) -> String {
    text.to_owned()
}

// ---------------------------------------------------------------------------
// Native HTTP runtime.
//
// ORDER MATTERS (integration contract with build.rs, owned by T11):
//   * build.rs writes `service_routes.rs` into OUT_DIR; the generator emits
//     `pub fn register_routes(router: axum::Router<crate::server::AppState>, port:
//     Option<u16>) -> axum::Router<crate::server::AppState>` plus one `#[path] mod` per
//     discovered route file (`build_support/emit.rs:63-78`, name from
//     `build_support/mod.rs::GENERATED_FUNCTION`). `port` is the listener being built
//     (`None` = the default one), and each route module mounts only the groups that
//     declare it (`src/server/metadata.rs`).
//   * The generated header names THIS file as the include site, at CRATE scope, and
//     the signatures use absolute `crate::server::AppState` paths so that is the
//     only place they read correctly. The include location and the call path are a
//     pair: `server::app` calls `crate::register_routes`, so moving this `include!`
//     into `server` means changing that call to `register_routes`
//     (unqualified/sibling) - they cannot drift apart or the route table disappears
//     from `app()`.
// Name resolution inside a crate is order-independent, so the include sitting
// above `pub mod server` is a readability choice, not a requirement.
// ---------------------------------------------------------------------------
include!(concat!(env!("OUT_DIR"), "/service_routes.rs"));

/// Shared state + the axum `Router` factory for the native runtime.
pub mod server;

/// The process's log surface: the hand-written `tracing::Subscriber`, the `RUST_LOG` level
/// filter, and the target the gateway's per-request line is emitted on. The executable installs
/// it (`src/bin/server.rs`); an embedder that serves `server::app` on its own listener can
/// install it too, or route the same events through its own subscriber.
pub mod logging;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_matches_the_naming_rule() {
        assert_eq!(CRATE_NAME, "typecrate_services");
    }

    #[test]
    fn version_matches_the_npm_line() {
        // Hardcoded on purpose: package.json "version" has to be edited in lockstep.
        assert_eq!(version(), "1.0.2");
    }

    #[test]
    fn echo_returns_its_input_verbatim() {
        assert_eq!(echo(""), "");
        assert_eq!(echo("identity"), "identity");
        // Multi-byte UTF-8 and a 4-byte codepoint: the exact bytes must survive.
        assert_eq!(echo("a - o - \u{1F600}"), "a - o - \u{1F600}");
    }
}
