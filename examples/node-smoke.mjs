// =============================================================================
// node-smoke.mjs - runs the generated WASM glue in Node and asserts the exact
// values the Rust side produces. This is the proof that the npm entry points are
// real: `pkg/typecrate_services.js` loads, `pkg/typecrate_services_bg.wasm`
// instantiates, and the three #[wasm_bindgen] exports return the same strings the
// native tests assert (src/lib.rs, tests/api.rs).
//
// Requires `npm run build:wasm` first - pkg/ is a build artifact, not a source.
// Run: npm run smoke:wasm
//
// WHY the wasm bytes are read by hand instead of passing a URL: the glue is
// `--target web` (rust/README.md:158-180) and Node's fetch cannot read `file:`
// URLs, so the documented Node pattern is init(readFileSync(...)) - where "init"
// is the glue's default export (the CLI 0.2.130 names that function
// `__wbg_init`, so it arrives as `mod.default`).
// =============================================================================

import assert from "node:assert/strict";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const gluePath = join(root, "pkg", "typecrate_services.js");
const wasmPath = join(root, "pkg", "typecrate_services_bg.wasm");
const typesPath = join(root, "pkg", "typecrate_services.d.ts");

if (!existsSync(gluePath) || !existsSync(wasmPath)) {
    console.error(`[smoke] missing ${gluePath} or ${wasmPath}\n[smoke] run: npm run build:wasm`);
    process.exit(2);
}

// Dynamic import so a broken/absent artifact reports here instead of as an
// unhandled ESM resolution error.
let mod;
try {
    mod = await import(pathToFileURL(gluePath).href);
} catch (err) {
    console.error(`[smoke] cannot import ${gluePath}: ${err.message}\n[smoke] run: npm run build:wasm`);
    process.exit(2);
}

// 1. Shape of the module: the default export is the init function (`__wbg_init`
//    in wasm-bindgen 0.2.130, `init` in older CLIs), and the Rust functions are
//    named exports. Taking `mod.default` first is what keeps this working with
//    the version Cargo.lock actually pins.
const init = mod.default ?? mod.init;
assert.equal(typeof init, "function", "expected the generated init/__wbg_init default export");
for (const name of ["crateName", "version", "echo"]) assert.equal(typeof mod[name], "function", `expected a ${name}() export`);

// 2. Instantiate with the bytes from disk (same artifact a browser fetches).
await init(readFileSync(wasmPath));

// 3. Exact values, identical to the Rust assertions in src/lib.rs.
assert.equal(mod.crateName(), "typecrate_services");
assert.equal(mod.version(), "1.0.2");
assert.equal(mod.echo(""), "");
assert.equal(mod.echo("identity"), "identity");
// Multi-byte + astral-plane codepoint: the glue's UTF-8 marshalling round-trips.
assert.equal(mod.echo("a - o - \u{1F600}"), "a - o - \u{1F600}");

// 4. The published declarations ("exports.types" -> pkg/typecrate_services.d.ts)
//    declare that same surface - a stale or empty .d.ts is a publish-blocking bug,
//    not a nit. Signatures are matched rather than whole files, and the search
//    covers every generated .d.ts, because the CLI's file split moves between
//    versions (declarations may live in the entry or in `${crate}_bg.d.ts` behind
//    a re-export).
if (!existsSync(typesPath) || readFileSync(typesPath, "utf8").trim().length === 0) {
    console.error(`[smoke] ${typesPath} is missing or empty\n[smoke] run: npm run build:wasm`);
    process.exit(2);
}
const typesDir = join(root, "pkg");
const types = readdirSync(typesDir)
    .filter((f) => f.endsWith(".d.ts"))
    .map((f) => readFileSync(join(typesDir, f), "utf8"))
    .join("\n");
for (const signature of [
    /export\s+function\s+crateName\s*\(\s*\)\s*:\s*string/,
    /export\s+function\s+version\s*\(\s*\)\s*:\s*string/,
    /export\s+function\s+echo\s*\(\s*text\s*:\s*string\s*\)\s*:\s*string/,
]) {
    assert.match(types, signature, `missing declaration among the generated .d.ts files in ${typesDir}`);
}
// The init function is what every host has to call, so it must be typed too, or
// the published entry point is untyped at the one call that matters. wasm-bindgen
// 0.2.130 (`--target web`, i.e. the version Cargo.lock pins) emits it on the
// entry as
//   export default function __wbg_init (module_or_path?: InitInput | Promise<InitInput>): Promise<InitOutput>;
// so `__wbg_init` is matched, with `init` kept as the older-CLI spelling rather
// than assuming one generator's layout.
assert.match(types, /export\s+(default\s+)?function\s+(__wbg_init|init)\s*\(/, `missing __wbg_init()/init() declaration in ${typesDir}`);

console.log(`[smoke] ok: ${Object.keys(mod).sort().join(", ")} - crateName/version/echo verified against pkg/*.js + pkg/*.d.ts`);
