//! Generated module identifiers: sanitise, disambiguate, never collide, never change.
//!
//! Identifiers are derived from the **scan-root-relative** path: unique before disambiguation,
//! readable (`endpoints/health/health-route.rs` -> `endpoints_health_health_route`),
//! and stable as long as the scan root and the tree shape stay where they are - which is why
//! `build.rs` prints the resolved root in the generated header (moving the root moves every
//! identifier).
//! Only [`ROUTE_FILE_EXTENSION`] (`.rs`) is stripped here, never the `-route` half of the
//! file name, so every discovered identifier ends in `_route` and reads like the file it
//! came from.
//! Hyphenated file and directory names are perfectly valid path segments - they are what
//! the contract uses - so the mapping hyphen -> underscore happens *only* in the
//! identifier, never in the `#[path]` value (`emit.rs` keeps the real name on disk).

use std::collections::HashSet;

use super::ROUTE_FILE_EXTENSION;
use super::ROUTE_FILE_SUFFIX;

/// Words a generated identifier must never equal, because `mod <keyword>;` is a parse
/// error. A produced name always carries the `_route` tail, so this is a guard
/// for pathological inputs (`module_name_for("mod.rs")`) rather than an expected case.
const RUST_KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "if", "impl", "import",
    "in", "let", "loop", "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "try", "type", "typeof", "union",
    "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// Turns one path segment into an identifier fragment.
///
/// Everything outside `[A-Za-z0-9_]` becomes `_` (including `-`, `.` and spaces), and a
/// non-ASCII letter becomes `_` too: staying ASCII-only means the identifier is valid
/// regardless of the `unicode-identifier` feature. A leading digit gets a `_` prefix
/// (Rust identifiers cannot start with one); an empty segment collapses to `_`.
#[must_use]
pub fn sanitize_segment(segment: &str) -> String {
    let cleaned: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = if cleaned.is_empty() { "_" } else { &cleaned };
    if cleaned.as_bytes()[0].is_ascii_digit() {
        format!("_{cleaned}")
    } else {
        cleaned.to_owned()
    }
}

/// Identifier for one scan-root-relative path (`/`-separated, `.rs` suffix stripped).
///
/// See the module docs for the sanitising rules; uniqueness is *not* provided here, use
/// [`assign_module_names`] for that.
#[must_use]
pub fn module_name_for(relative_path: &str) -> String {
    let mut parts: Vec<&str> = relative_path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if let Some(file_name) = parts.last_mut() {
        // Only the `.rs` extension is stripped, never the `-route` half of the name: the
        // identifier keeps that and therefore ends in `_route` like its file does.
        *file_name = file_name
            .strip_suffix(ROUTE_FILE_EXTENSION)
            .unwrap_or(file_name);
    }
    let fragments: Vec<String> = parts.iter().map(|part| sanitize_segment(part)).collect();
    let joined = if fragments.is_empty() {
        // Degenerate input (empty relative path): fall back to the route suffix minus its
        // extension (`-route` -> `_route`), so the identifier still names the kind of file it
        // came from instead of collapsing to an empty one.
        sanitize_segment(
            ROUTE_FILE_SUFFIX
                .strip_suffix(ROUTE_FILE_EXTENSION)
                .unwrap_or(ROUTE_FILE_SUFFIX),
        )
    } else {
        fragments.join("_")
    };
    // `mod <keyword>;` would not parse; `_`-prefixing keeps the name unique-friendly.
    if RUST_KEYWORDS.contains(&joined.as_str()) {
        format!("_{joined}")
    } else {
        joined
    }
}

/// Unique, deterministic identifiers, index-aligned with `relative_paths`.
///
/// The input order is the registration order, so the *first* path in that order keeps the
/// clean name and later collisions get `_2`, `_3`, ... Two distinct paths can sanitise to
/// the same fragment (`endpoints/a-b/service-route.rs` and `endpoints/a_b/service-route.rs`
/// both map to `endpoints_a_b_service_route`; on a case-insensitive filesystem
/// `Health`/`health` do the same), which is exactly what this pass exists to resolve.
#[must_use]
pub fn assign_module_names(relative_paths: &[String]) -> Vec<String> {
    let mut taken: HashSet<String> = HashSet::with_capacity(relative_paths.len());
    let mut names: Vec<String> = Vec::with_capacity(relative_paths.len());
    for relative_path in relative_paths {
        let base = module_name_for(relative_path);
        let mut candidate = base.clone();
        let mut suffix: usize = 1;
        while taken.contains(&candidate) {
            suffix += 1;
            candidate = format!("{base}_{suffix}");
        }
        taken.insert(candidate.clone());
        names.push(candidate);
    }
    names
}

/// Whether an identifier needs `#[allow(non_snake_case)]` in front of its generated `mod`
/// declaration (F2 - the generated file is `include!`d into the crate, so
/// `npm run lint:strict` denies this lint exactly like a hand-written line).
///
/// The rule was **measured**, not guessed: rustc 1.99.1 with `-D non-snake-case`
/// (`Temp/opencode/t14/lint-probe.rs`, `lint-probe2.rs`) rejects a module identifier that
/// carries an *uppercase* letter or a *doubled separator between words*, and accepts
/// everything else: `a__b`, `a___b`, `a__1_b` fail; `a_2_b`, `a_2b`, `a2_b`, `_a_b`, `a_b_`,
/// `a_b__` pass. Two ordinary path shapes reach those forms while remaining valid
/// identifiers: a digit-leading segment (`endpoints/2024/` -> `_2024`, and the `_` joiner
/// adds a second separator) and a segment with capitals or non-ASCII letters (`Health/`,
/// `Ünïcode/` -> `_n_code`).
///
/// Deliberately an **over-approximation** (leading/trailing `_` runs also match, and the
/// `_2` disambiguator after a name already ending in `_` can too): a needless allow only
/// relaxes a style lint, while a missing one breaks the strict-lint gate. It is applied per
/// declaration rather than globally because an attribute on a `#[path]` module also covers
/// that module's *body* - a clean route file keeps the full lint over its own code.
#[must_use]
pub fn needs_snake_case_allow(name: &str) -> bool {
    name.contains("__") || name.chars().any(|c| c.is_ascii_uppercase())
}
