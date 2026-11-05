//! `typecrate_services` - Rust-native core behind the `@typecrate/services` npm package.
//!
//! Two consumers, ONE implementation (see `Cargo.toml` for why this is a
//! standalone workspace sitting in the npm package directory):
//!
//! * native (default): host builds link this rlib crate-to-crate - no wasm, no
//!   serialisation, no JavaScript in the loop. This is the primary path, and since
//!   T10 it also includes the native HTTP runtime (`server`, plus the
//!   `typecrate-services-server` executable in `src/bin/server.rs`).
//! * JS boundary (optional): `cargo build --target wasm32-unknown-unknown` plus
//!   `wasm-bindgen --target web` (driven by `scripts/build-wasm.mjs`) publish the
//!   same functions to Node/browser as `pkg/typecrate_services.js` + the
//!   generated `pkg/typecrate_services.d.ts`.
//!
//! CURRENT STATE: identity-level core (`CRATE_NAME`, `version`, `echo`) - the API
//! the TS service and the wasm glue already expose - plus the native server
//! runtime below. `echo` and the wasm surface are unchanged; the runtime is
//! additive and compiled only for non-wasm targets, so `npm run wasm:check` and
//! `npm run build:wasm` keep compiling the same wasm module as before.

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
/// and a real return value across every boundary this package has (Rust caller,
/// wasm caller). Any behaviour other than "the same bytes came back" is a bug in
/// the boundary, not in this function.
#[must_use]
pub fn echo(text: &str) -> String {
    text.to_owned()
}

// ---------------------------------------------------------------------------
// Native HTTP runtime: the whole block below is compiled only when the target is
// NOT wasm32-unknown-unknown, which is the exact inverse of the wasm-bindgen gate
// in Cargo.toml. axum/tokio are target-gated the same way, so a wasm build sees
// neither the module nor its dependencies (tokio has no IO/time driver there).
//
// ORDER MATTERS (integration contract with build.rs, owned by T11):
//   * build.rs writes `service_routes.rs` into OUT_DIR; the generator emits
//     `pub fn register_routes(router: axum::Router<crate::server::AppState>) ->
//     axum::Router<crate::server::AppState>` plus one `#[path] mod` per discovered
//     route file (`build_support/emit.rs:63-78`, name from
//     `build_support/mod.rs::GENERATED_FUNCTION`).
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
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
include!(concat!(env!("OUT_DIR"), "/service_routes.rs"));

/// Shared state + the axum `Router` factory for the native runtime.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub mod server;

// ---------------------------------------------------------------------------
// wasm boundary: compiled ONLY for wasm32-unknown-unknown, so a native build
// never pulls wasm-bindgen in (the dependency is target-gated in Cargo.toml).
// These are thin `String`/`&str` adapters over the native functions above; the
// `js_name` spellings are the camelCase JS surface, and they are what
// `scripts/build-wasm.mjs` and `examples/node-smoke.mjs` expect to find in the
// generated glue.
// ---------------------------------------------------------------------------
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod wasm {
    use wasm_bindgen::prelude::*;

    /// Rust-side crate identifier, for hosts that want to log which module they loaded.
    #[wasm_bindgen(js_name = crateName)]
    #[must_use]
    pub fn crate_name() -> String {
        super::CRATE_NAME.to_owned()
    }

    /// Crate version == the published npm version.
    #[wasm_bindgen(js_name = version)]
    #[must_use]
    pub fn version() -> String {
        super::version().to_owned()
    }

    /// `echo` across the JS boundary: proves the generated glue marshals strings
    /// (including non-ASCII) without a second implementation.
    #[wasm_bindgen(js_name = echo)]
    #[must_use]
    pub fn echo(text: &str) -> String {
        super::echo(text)
    }
}

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
