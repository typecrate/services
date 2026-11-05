// =============================================================================
// build-wasm.mjs - the WASM half of @typecrate/services: Rust artifact -> JS glue
// + generated TypeScript declarations, i.e. exactly what the npm entry points
// ("exports.import" / "exports.types") must find in pkg/.
//
// Two steps, because `wasm-bindgen` is NOT a cargo subcommand (rustup does not
// manage it either - rust/README.md:150-156):
//   1. cargo build --release --locked --target wasm32-unknown-unknown
//      (TYPECRATE_WASM_DEBUG=1 drops --release; cargo has no --debug flag)
//        -> <target-dir>/wasm32-unknown-unknown/<profile>/typecrate_services.wasm
//   2. wasm-bindgen --target web --out-dir pkg --out-name typecrate_services <wasm>
//        -> pkg/typecrate_services.js          (ESM glue, the npm "import" entry;
//                                                its default export is the init
//                                                function - `__wbg_init` in 0.2.130)
//           pkg/typecrate_services.d.ts        (generated types, the npm "types" entry)
//           pkg/typecrate_services_bg.wasm     (the module itself)
//           pkg/typecrate_services_bg.wasm.d.ts
//
// WHY `--target web` (not `bundler`, not `nodejs`): rust/README.md:158-180 - one
// ESM artifact for browser AND Node, whose init function accepts a URL, a
// Response, a BufferSource, ... ; `--target nodejs` emits CommonJS the browser
// build cannot use and `--target bundler` would force vite plugins. `wasm-pack`
// is unmaintained by the wasm-bindgen team and would overwrite the hand-authored
// package.json.
//
// Design rules this script enforces (each one fails loudly instead of guessing):
//   * paths come from `cargo metadata`, so the repo-root `.cargo/config.toml`
//     `target-dir = ".rust/target"` is honoured wherever the repo is cloned;
//   * the CLI version must EQUAL the `wasm-bindgen` crate in `Cargo.lock` - the
//     generated loader embeds a version check and panics on any drift, so the
//     pin is read from the lock (never hardcoded, never guessed);
//   * no tool is installed or downloaded silently: a missing CLI is an error with
//     the exact install command / prebuilt release URL for this platform;
//   * the generated files and the exported names are re-read after generation, so
//     `npm pack` (prepack -> this script) can never publish entry paths that do
//     not exist or a module that exports nothing.
// =============================================================================

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
/** The npm package == the Cargo workspace root (single combined crate). */
const root = resolve(here, "..");
const manifest = join(root, "Cargo.toml");
const lockPath = join(root, "Cargo.lock");
const pkgDir = join(root, "pkg");

const CRATE = "typecrate_services";
const TRIPLE = "wasm32-unknown-unknown";
// `TYPECRATE_WASM_DEBUG=1` builds the unoptimised artifact (fat module, fast loop).
const PROFILE = process.env.TYPECRATE_WASM_DEBUG === "1" ? "debug" : "release";
const LOG = "[typecrate-services:wasm]";

// ---------------------------------------------------------------- repo layout
// AGENTS.md: child processes must not fall back to OS temp. The root
// .cargo/config.toml already redirects cargo's own children (TMP/TEMP/TMPDIR ->
// Temp/rust); that is per-cargo-invocation, so the CLI spawned below (and cargo
// metadata) get the same pin here, for THIS process only.
function repoRoot() {
    // Nearest ancestor holding .cargo/config.toml == the repo root by this repo's
    // own layout; fall back to the historical 3-levels-up position.
    for (let dir = root, i = 0; i < 8; i += 1, dir = dirname(dir)) {
        if (existsSync(join(dir, ".cargo", "config.toml"))) return dir;
        const parent = dirname(dir);
        if (parent === dir) break;
    }
    return resolve(root, "..", "..", "..");
}

const repo = repoRoot();
const scratch = join(repo, "Temp");
mkdirSync(scratch, { recursive: true });
for (const key of ["TEMP", "TMP", "TMPDIR"]) process.env[key] = scratch;

// ------------------------------------------------------------------ utilities
function fail(message, hint) {
    console.error(`\n${LOG} ${message}`);
    if (hint) console.error(`${LOG} ${hint}\n`);
    process.exit(1);
}

/**
 * Spawn a tool. `spawned:false` means the binary does not exist (ENOENT) - kept
 * separate from a non-zero exit, because those need different advice.
 */
function run(binary, args, { inherit = false } = {}) {
    const res = spawnSync(binary, args, {
        cwd: root,
        encoding: inherit ? undefined : "utf8",
        stdio: inherit ? "inherit" : ["ignore", "pipe", "pipe"],
    });
    return { spawned: res.error === undefined, status: res.status ?? 1, stdout: res.stdout ?? "" };
}

/** True only if the tool exists AND ran successfully - the "is it installed" probe. */
function ok(result) {
    return result.spawned && result.status === 0;
}

// ------------------------------------------------------- cargo + artifact path
// `cargo metadata` is the only portable way to learn where the artifact lands:
// the target directory comes from config (repo-root `.cargo/config.toml` sets
// `.rust/target`) and may be overridden by CARGO_TARGET_DIR / --target-dir.
function targetDirectory() {
    // `--offline` first: with a committed Cargo.lock + warm cache it answers
    // without touching the network; the plain call is the fallback for a cold
    // cache, where resolution has to reach the registry.
    for (const extra of [["--offline"], []]) {
        const res = run("cargo", ["metadata", "--format-version", "1", "--no-deps", "--manifest-path", manifest, ...extra]);
        if (!res.spawned) {
            fail(
                "cargo is not on PATH",
                "install the pinned toolchain: `rustup toolchain install` (rust-toolchain.toml pins 1.99.1 + wasm32-unknown-unknown)",
            );
        }
        if (res.status !== 0) continue;
        try {
            const meta = JSON.parse(res.stdout);
            if (meta.target_directory) return meta.target_directory;
        } catch {
            /* malformed output: fall through to the next resolution mode */
        }
    }
    fail("could not read target_directory from `cargo metadata`", "set CARGO_TARGET_DIR, or point TYPECRATE_WASM_DIR at the directory holding the .wasm");
    return null;
}

const wasmIn =
    process.env.TYPECRATE_WASM_DIR !== undefined
        ? resolve(process.env.TYPECRATE_WASM_DIR, `${CRATE}.wasm`)
        : join(targetDirectory(), TRIPLE, PROFILE, `${CRATE}.wasm`);

// ---------------------------------------------------------- wasm-bindgen CLI
// The CLI ships as `wasm-bindgen` (some distros name it `wasm-bindgen-cli`); the
// documented local install location for this repo is `<repo>/.rust/bin`.
function resolveBindgen() {
    const candidates = [];
    if (process.env.TYPECRATE_WASM_BINDGEN) {
        const override = process.env.TYPECRATE_WASM_BINDGEN;
        candidates.push({ label: override, probe: override });
    }
    // Bare names: resolved through PATH by the OS, including the .exe spelling.
    for (const name of ["wasm-bindgen", "wasm-bindgen.exe", "wasm-bindgen-cli"]) candidates.push({ label: name, probe: name });
    // rust/README.md:150-156 documents .rust/bin as this repo's local tool home.
    for (const exe of ["wasm-bindgen.exe", "wasm-bindgen"]) {
        const path = join(repo, ".rust", "bin", exe);
        if (existsSync(path)) candidates.push({ label: path, probe: path });
    }
    for (const c of candidates) {
        const res = run(c.probe, ["--version"]);
        if (!ok(res)) continue;
        const version = (/\d+\.\d+\.\d+/.exec(res.stdout) ?? [null])[0];
        if (version) return { bin: c.probe, name: c.label, version };
    }
    return null;
}

/** The `wasm-bindgen` library version this workspace resolved into Cargo.lock. */
function lockedBindgenVersion() {
    if (!existsSync(lockPath)) return null;
    const entry = /name = "wasm-bindgen"\r?\nversion = "([^"]+)"/.exec(readFileSync(lockPath, "utf8"));
    return entry ? entry[1] : null;
}

/** Best-matching prebuilt CLI release for this platform (rust/README.md:150-156). */
function prebuiltUrl(version) {
    const triples = {
        "win32-x64": "x86_64-pc-windows-msvc",
        "linux-x64": "x86_64-unknown-linux-gnu",
        "linux-arm64": "aarch64-unknown-linux-gnu",
        "darwin-x64": "x86_64-apple-darwin",
        "darwin-arm64": "aarch64-apple-darwin",
    };
    const triple = triples[`${process.platform}-${process.arch}`] ?? "x86_64-unknown-linux-gnu";
    return `https://github.com/rustwasm/wasm-bindgen/releases/download/${version}/wasm-bindgen-${version}-${triple}.zip`;
}

// --------------------------------------------------------------- 1. wasm build
// Done here (not in a separate npm script) so `npm run build:wasm` and `prepack`
// can never generate glue from a stale or missing artifact.
//
// Profile spelling, the way cargo actually accepts it: there is NO `--debug` flag
// (debug is the default profile), and `cargo build --debug` is a hard error -
// which is what `TYPECRATE_WASM_DEBUG=1` used to hit before it reached the
// wasm-bindgen check. Only the release profile contributes a flag.
//
// `--locked` is what makes the committed Cargo.lock promise real: the wasm build,
// like every cargo script in package.json, refuses to silently re-resolve
// dependencies. A missing or stale lock stops with cargo's own message ("run
// `cargo generate-lockfile` to update") instead of drifting the wasm-bindgen
// version this script has to read out of that same lock.
const cargoArgs = ["build", ...(PROFILE === "release" ? ["--release"] : []), "--locked", "--target", TRIPLE, "--manifest-path", manifest];
console.log(`${LOG} cargo ${cargoArgs.join(" ")}`);
{
    const res = run("cargo", cargoArgs, { inherit: true });
    if (!res.spawned) fail("cargo is not on PATH", "install the pinned toolchain: `rustup toolchain install`");
    if (res.status !== 0) fail(`cargo build --target ${TRIPLE} failed (exit ${res.status})`, "run `npm run wasm:check` to see the compiler diagnostics");
}
if (!existsSync(wasmIn)) {
    fail(
        `no wasm module at ${wasmIn}`,
        `expected it under the cargo target directory for ${TRIPLE}/${PROFILE}; override the lookup with TYPECRATE_WASM_DIR=<dir>`,
    );
}

// -------------------------------------------------------------- 2. CLI + glue
const pinned = lockedBindgenVersion();
if (pinned === null) {
    // Reading the pin requires the lock; the lock is cargo's, never hand-authored
    // here, so an absent lock is a hard stop rather than a licence to guess.
    fail(`no wasm-bindgen entry in ${lockPath}`, "run `npm run check` first - cargo writes Cargo.lock on its first run");
}

const bindgen = resolveBindgen();
if (bindgen === null) {
    fail(
        "wasm-bindgen CLI is not installed (this script never installs or downloads a tool)",
        `pin it to the lockfile version ${pinned}: ` +
            `\`cargo install wasm-bindgen-cli --locked --version ${pinned}\`, or unpack the prebuilt release into ${join(repo, ".rust", "bin")}: ${prebuiltUrl(pinned)}. ` +
            `An unusual location is fine: point TYPECRATE_WASM_BINDGEN at the binary.`,
    );
}
// A CLI that exists at the wrong version otherwise fails later, inside the
// generated loader, with a message nobody connects back to this script.
if (bindgen.version !== pinned) {
    fail(
        `${bindgen.name} is ${bindgen.version} but Cargo.lock pins the wasm-bindgen library at ${pinned} (they must match exactly)`,
        `\`cargo install wasm-bindgen-cli --locked --version ${pinned}\``,
    );
}

const outFiles = [`${CRATE}.js`, `${CRATE}.d.ts`, `${CRATE}_bg.wasm`];
console.log(`${LOG} ${bindgen.name} ${bindgen.version} --target web -> ${pkgDir}`);
{
    const res = run(bindgen.bin, ["--target", "web", "--out-dir", pkgDir, "--out-name", CRATE, wasmIn], { inherit: true });
    if (!res.spawned) fail(`could not exec ${bindgen.name}`, "TYPECRATE_WASM_BINDGEN must point at an executable, not a directory");
    if (res.status !== 0) fail(`${bindgen.name} --target web failed (exit ${res.status})`, `artifact: ${wasmIn}`);
}

const missing = outFiles.filter((f) => !existsSync(join(pkgDir, f)));
if (missing.length > 0) {
    fail(`wasm-bindgen did not produce ${missing.join(", ")} in pkg/`, `pkg/ contains: ${readdirSync(pkgDir).join(", ") || "nothing"}`);
}
// The npm "import"/"types" entries point at those two files, and a module that
// exports nothing is publishable but useless, so the generated surface is read
// back here. Names are looked for across ALL generated JS/TS files rather than
// only the entry file: the CLI's file split has moved between versions (the
// entry sometimes inlines the bindings, sometimes re-exports them from
// `${CRATE}_bg.js`), and examples/node-smoke.mjs is what proves the entry
// actually *reaches* them at runtime.
const generated = readdirSync(pkgDir).filter((f) => f.endsWith(".js") || f.endsWith(".d.ts"));
const read = (f) => readFileSync(join(pkgDir, f), "utf8");
const glue = generated.filter((f) => f.endsWith(".js") && !f.endsWith(".d.ts")).map(read).join("\n");
const types = generated.filter((f) => f.endsWith(".d.ts")).map(read).join("\n");
for (const exported of ["crateName", "version", "echo"]) {
    if (!new RegExp(`export\\s+function\\s+${exported}\\b`).test(glue)) {
        fail(`no generated JS exports function ${exported}()`, "the #[wasm_bindgen] surface in src/lib.rs and this check have drifted");
    }
    if (!new RegExp(`export\\s+function\\s+${exported}\\s*\\(`).test(types)) {
        fail(`no generated .d.ts declares ${exported}()`, "npm consumers would get an untyped entry point");
    }
}
if (types.trim().length === 0 || glue.trim().length === 0) {
    fail("generated glue/declarations are empty", `pkg/ contains: ${generated.join(", ")}`);
}
// No nested `pkg/package.json` (unlike packages/underload/nexus): that is needed
// for `--target nodejs` CommonJS glue, while this package is "type": "module" and
// `--target web` emits ESM, so Node parses pkg/*.js correctly as-is.
console.log(`${LOG} ok: exports crateName, version, echo (js + d.ts) -> ${pkgDir}`);
console.log(`${LOG} pkg/: ${readdirSync(pkgDir).join(", ")}`);
console.log(`${LOG} next: npm run smoke:wasm`);
