//! Native-consumption check: an integration test is compiled as a *separate
//! crate* that links `typecrate_services` as an rlib, so this file proves the
//! primary (Rust-first, no wasm, no JS) consumption path and the public surface
//! a downstream crate would actually see.

use typecrate_services::{CRATE_NAME, echo, version};

#[test]
fn public_surface_is_reachable_from_another_crate() {
    assert_eq!(CRATE_NAME, "typecrate_services");
    assert_eq!(version(), "1.0.2");
    assert_eq!(echo("native"), "native");
}
