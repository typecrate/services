# @typecrate/services

Rust-first service core for the `typecrate` scope. The crate in this directory is
the implementation; **native Rust is the default** (build / check / test / lint /
run are all `cargo`, with no JS toolchain involved). JavaScript consumers get an
**optional** WASM build: `cargo` produces the module and `wasm-bindgen --target web`
generates the JS glue plus the TypeScript declarations that the npm entry points
point at.

Current phase: the service **framework** is in place — a runnable native HTTP
executable, build-time route discovery, shared state, and a sample endpoint set of
**fifteen routes**: `health`, `echo`, and two REST samples, `locations` and `pets` (see
[Endpoints as they behave on the wire](#endpoints-as-they-behave-on-the-wire)). The
server also owns four **framework builtins** on the same origin: the interactive
playground at `GET /` (plus its embedded `GET /playground.css` / `GET /playground.js`)
and the JSON endpoint listing at `GET /v1/services/routes` (see
[Where the server answers](#where-the-server-answers)).
The existing TypeScript service endpoints are **not ported
yet**; see [Scope and parity gaps](#scope-and-parity-gaps).

## Layout

```
packages/typecrate/services/
  package.json              # npm identity + cargo-driven scripts (hand-authored)
  Cargo.toml                # STANDALONE Cargo workspace + the `typecrate-services` crate
  Cargo.lock                # committed: this workspace has its own lock (root lock is separate)
  build.rs                  # build-time scan of `*-route.rs` files -> $OUT_DIR/service_routes.rs
  build_support/            # the scanner + generator rules (shared with tests/discovery.rs)
  src/lib.rs                # Rust core + the generated route table + the wasm boundary
  src/server/mod.rs         # AppState (shared state) + the axum `Router` factory + the embedded browser surface
  src/server/metadata.rs    # `EndpointMeta` + the `crate::service_routes!` macro (one entry -> route AND listing)
  src/server/playground.*   # the page/css/js `include_str!`d into the binary (served at `/`, `/playground.css`, `/playground.js`)
  src/bin/server.rs         # the `typecrate-services-server` executable (CLI, bind, banner, shutdown)
  examples/endpoints/**     # the sample endpoint trees: one lightweight `<resource>-route.rs`
                            # plus one `<resource>-core.rs` implementation file per resource
  tests/api.rs              # native-consumption test (links the rlib like a downstream crate)
  tests/discovery.rs        # scanner/generator rules against real fixture trees
  tests/server_http.rs      # real-TCP HTTP tests of the exact library router
  scripts/build-wasm.mjs    # wasm artifact -> pkg/ glue + generated .d.ts
  scripts/discover-services.mjs # pre-step: tree-shape snapshot -> Temp/ watched file
  scripts/check-playground.mjs  # optional jsdom check of the embedded browser surface
  examples/node-smoke.mjs   # runs pkg/ glue in Node and asserts exact values
  pkg/                      # generated (git-ignored): JS + .d.ts + .wasm
```

The one artifact outside the package is the discovery snapshot,
`<repo>/Temp/typecrate-services/route-discovery.json`: git-ignored scratch that
`build.rs` watches so a new endpoint folder is picked up by the next `npm` run (see
[Rebuild tracking](#rebuild-tracking-what-re-runs-the-scan-and-what-needs-a-re-scan)).

`Cargo.toml` declares its own `[workspace]` on purpose: the package lives where the
npm package lives (`packages/typecrate/services`), while the root workspace only
globs `rust/crates/*`. The explicit boundary is what stops cargo from erroring
`current package believes it's in a workspace when it's not` — the same shape
`packages/underload/nexus` uses. Toolchain and cargo config still come from the
repo root by directory walk-up: `rust-toolchain.toml` pins **1.99.1 +
wasm32-unknown-unknown**, and `.cargo/config.toml` sets `target-dir = .rust/target`
plus `TMP/TEMP/TMPDIR = Temp/rust`.

Naming: crate `typecrate-services` == npm `@typecrate/services` with `@` dropped
and `/` replaced by `-` (rust/README.md naming rule); in Rust code it is
`use typecrate_services::...`. Version `1.0.2` is the single line for both —
`tests::version_matches_the_npm_line` fails if they drift.

## Native service (default)

```bash
yarn workspace @typecrate/services run-native    # discovery snapshot, then cargo run --bin typecrate-services-server --locked --
yarn workspace @typecrate/services build         # discovery snapshot, then cargo build --release --locked -> the same executable
```

`run-native` (alias: `start`) ends in a bare `--`, so anything after npm's own `--`
separator reaches the executable:

```bash
npm start -- --port 4321 --database-root D:\data         # LAN-reachable on :4321
npm start -- --addr 127.0.0.1                            # the loopback opt-in
cargo run --bin typecrate-services-server --locked -- --help   # the same, without npm
```

| Setting | Flag | Environment | Default |
| --- | --- | --- | --- |
| Bind address | `-a, --addr <ip[:port]>`, `--port <port>` | `TYPECRATE_SERVICES_ADDR` | `0.0.0.0:5001` |
| Writable data root | `--database-root <dir>` | `TYPECRATE_SERVICES_DATABASE_ROOT` | `<repo-parent>/temporary/dump` |
| Request body cap | — (constant) | — | `1 MiB` (`server::MAX_REQUEST_BODY_BYTES`) |

Precedence per setting: **flag, then environment, then built-in default**. A bare
`--addr` keeps the already-selected port, `--port` keeps the already-selected host,
and an IPv6 address is written `[::1]:7007`.

Security posture, deliberately:

- **reachable from the LAN by default** — `0.0.0.0:5001` binds every interface, the
  same surface the TS gateway publishes, so a phone or a second machine can open the
  playground without configuration. `--addr 127.0.0.1` (or
  `TYPECRATE_SERVICES_ADDR=127.0.0.1:5001`) is the opt-in for a machine that must
  stay loopback-only.
- **which URL to use** — `http://127.0.0.1:5001/` on the machine that runs the
  server (`0.0.0.0` is a bind answer, not a URL), `http://<this machine's LAN
  ip>:5001/` from anything else. The startup banner prints the loopback URLs plus the
  LAN address it detects, and the explicit `--addr <ip>` form pins the answer when
  the host has several interfaces (VPN, dock, second NIC).
- **binding is only half of reachability** — Windows Firewall, the network profile
  and the routing decide whether an inbound connection is accepted, so the first
  start may need a firewall permission prompt accepted. The server changes no
  firewall rule and no global network setting itself, and it cannot guarantee that
  another device gets through: that is a machine/network property, asserted by the
  banner wording rather than by a test.
- **trusted networks only** — requests are unauthenticated and there is no
  Origin/CORS gate, so this server belongs on a trusted LAN, not on the public
  internet (no reverse proxy or tunnel in front of it yet).
- **plain HTTP, no TLS** — the listener answers HTTP/1.1 in the clear, so a browser
  shows its usual "Not secure" warning at `http://<LAN ip>:5001/` and an HTTPS-only
  page cannot call it. Cosmetic for a LAN tool, but it also means anything typed into
  the playground — a header value, a body — crosses the network unencrypted, so do not
  send a secret you would not show a colleague on the same Wi-Fi.
- **one body cap, applied by the server** — an oversized POST is refused with
  `413 Failed to buffer the request body: length limit exceeded` before any
  endpoint parses it, so no route can forget to limit itself.
- **no filesystem path in any response** — `AppState.database_root` is server-side
  configuration; the sample endpoints answer with their own JSON fields only
  (`service`/`version` for `health`, `value` for `echo`, the pet/location record for the
  REST samples). The one path-like value a response carries is the `location` header of a
  `201`, and that is a URL path on this same origin, not a directory below the root.
- **nothing is ever killed** — a start fails with exit 1 only when **that address and
  port** are already bound (`cannot bind 0.0.0.0:5001 - address already in use`), never
  by killing somebody. "That address" matters on Windows: a bind to a *specific*
  interface can still succeed while another process holds the wildcard on the same port,
  e.g. a second instance started with `--addr 127.0.0.1 --port 5001` next to a running
  `0.0.0.0:5001`. Both are then listening, and the more specific socket wins the
  connections for its interface, so the new instance silently takes loopback while the
  older one keeps the LAN address. That is a valid setup only when it is the one you
  meant: **run parallel instances on distinct `--port` values** (the banner always prints
  the address and interface count this process actually got).

What a default start prints (`src/bin/server.rs`; the four address-facing lines are
asserted line-by-line in its unit tests, while the LAN line is whatever the interface
probe found and stays a placeholder when the host has no routable address):

```text
typecrate-services-server listening on 0.0.0.0:5001 - every interface
  database root:    <repo-parent>/temporary/dump
  max request body: 1048576 bytes
  routes:           generated at build time (Ctrl+C to stop)
  playground:       http://127.0.0.1:5001/
  endpoint list:    http://127.0.0.1:5001/v1/services/routes
  other devices:    http://192.168.1.20:5001/
  note:             unauthenticated - trusted LAN only, never the public internet; Windows Firewall may prompt on first start
```

A `--addr 127.0.0.1` start replaces the last two lines with
`other devices: none - bound to 127.0.0.1 (loopback only)` and a
`next: --addr 0.0.0.0 (the default) publishes :5001 on every interface` hint.

Exit codes: `0` clean shutdown (Ctrl+C, SIGTERM on unix, after draining in-flight
requests), `1` could not serve, `2` bad usage (`--help` prints usage and exits `0`).
`--help` documents the same posture offline (`WHERE IT ANSWERS` / `SECURITY`).

**Two different roots, on purpose.** The *scanner* root (where `*-route.rs` files
live, see below) and the *data* root (where endpoints create their own
folders) are separate directories: the tree being scanned never becomes a dump
directory. The data root matches the TypeScript gateway's boundary
(`packages/underload/service/server.ts:16`, `ROOT/../temporary/dump`) and is never
created eagerly — whoever writes first creates its own folder below it. The first
endpoints that actually write are the two REST samples, `pets` and `locations`, and their
exact files are documented in
[Endpoints as they behave on the wire](#endpoints-as-they-behave-on-the-wire).

### Where the server answers

Four paths belong to the **framework**, not to an endpoint file: `server::app`
registers them itself, and an endpoint must not take any of them (they are reserved
paths — the router is built in one `app()` call, so a discovered route that claims one
of them panics at startup instead of shadowing it; the exact responses are covered by
the package's HTTP tests, `tests/server_http.rs`).

| Method | Path | What it is |
| --- | --- | --- |
| `GET` | `/` | the interactive playground: one embedded HTML page, served from **this** origin with **no external assets** (no CDN, no separate static mount), which then calls the endpoints on the same origin |
| `GET` | `/playground.css` | the page's stylesheet, `include_str!`d into the executable (the document carries no inline `<style>`, which is what keeps the strict CSP honest) |
| `GET` | `/playground.js` | the page's script, `include_str!`d into the executable (no inline `<script>`, and the DOM is built with `textContent`, never `innerHTML`) |
| `GET` | `/v1/services/routes` | JSON listing of the compiled-in endpoints — one entry per registered route, each carrying `method`, `path` and `summary`, i.e. exactly what the playground offers |

```bash
curl -i http://127.0.0.1:5001/                     # playground HTML (same origin)
curl -i http://127.0.0.1:5001/playground.css       # the embedded stylesheet
curl -i http://127.0.0.1:5001/playground.js        # the embedded script
curl -s http://127.0.0.1:5001/v1/services/routes   # the endpoint listing
```

Every one of those four responses carries `Cache-Control: no-store` (they are compiled
into the binary, so a cached copy is a different version of the tool),
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` and a
`Content-Security-Policy` of `default-src 'self'` with **no `'unsafe-inline'`** and no
second origin — the playground therefore works on a LAN with no internet, and a
response body full of markup is displayed as text rather than executed.

The listing is the compiled-in set only, so today it is this package's fifteen sample
routes — the two `echo` ones, `health`, and the six routes each REST sample compiles in
(`locations` and `pets`, one row per `GET`/`POST`/`GET`/`PUT`/`PATCH`/`DELETE`) — the
`@underload/service` endpoints are still only on the TS gateway. Its rows are
**generated**, not hand-maintained: each route file declares its endpoints once through
`crate::service_routes!`, which emits both the `register` fn and that module's
`ENDPOINTS` slice from the same tokens (see
[Routes are compiled in at build time](#routes-are-compiled-in-at-build-time)), and the
generated `route_metadata()` concatenates those slices in registration order. A row is
`method`/`path`/`summary` only — no module identifier, no scan root, no `#[path]` — so
the listing is safe to serve on the LAN. The four builtins above are deliberately not
part of it: the listing answers "what endpoints does this build compile in", not "what
does the socket accept".

That answer in full, in registration order. On the wire it is one compact array; it is
split here one row per line so the whole set is readable, and every summary is verbatim
the text its route file declares:

```json
[{"method":"POST","path":"/v1/services/echo","summary":"Echoes the posted `value` unchanged"},
{"method":"GET","path":"/v1/services/echo/{value}","summary":"Echoes `{value}`, with `?prefix=` applied"},
{"method":"GET","path":"/v1/services/health","summary":"Reports the service name and version"},
{"method":"GET","path":"/v1/services/locations","summary":"Lists every stored location, `?city=` narrows by exact city"},
{"method":"POST","path":"/v1/services/locations","summary":"Creates a location and answers 201 with it"},
{"method":"GET","path":"/v1/services/locations/{id}","summary":"Returns one location by id, or 404 when the id is unknown"},
{"method":"PUT","path":"/v1/services/locations/{id}","summary":"Replaces one location by id, or 404 when the id is unknown"},
{"method":"PATCH","path":"/v1/services/locations/{id}","summary":"Updates only the sent fields of one location, or 404 when the id is unknown"},
{"method":"DELETE","path":"/v1/services/locations/{id}","summary":"Removes one location by id and answers 204, or 404 when the id is unknown"},
{"method":"GET","path":"/v1/services/pets","summary":"Lists every stored pet"},
{"method":"POST","path":"/v1/services/pets","summary":"Creates a pet and answers 201 with it"},
{"method":"GET","path":"/v1/services/pets/{id}","summary":"Returns one pet by id, or 404 when the id is unknown"},
{"method":"PUT","path":"/v1/services/pets/{id}","summary":"Replaces one pet by id, or 404 when the id is unknown"},
{"method":"PATCH","path":"/v1/services/pets/{id}","summary":"Updates only the sent fields of one pet, or 404 when the id is unknown"},
{"method":"DELETE","path":"/v1/services/pets/{id}","summary":"Removes one pet by id and answers 204, or 404 when the id is unknown"}]
```

The order is the scanner's byte-wise order of the route files, so
`endpoints/echo/echo-route.rs` comes first, then `health-route.rs`, `locations-route.rs`
and `pets-route.rs` — the `*-core.rs` files sitting next to them are never scanned, so they
cannot change this order; inside one route file it is the order of the
`crate::service_routes!` entries. And the set is not only prose:
`scripts/check-playground.mjs` pins it row-by-row in rail order (`LISTING_JSON`), so
`npm run check:ui -- --base <origin>` fails the moment a running server lists a different
method, path or summary.

From another device the same URLs are
`http://<this machine's LAN ip>:5001/` and
`http://<this machine's LAN ip>:5001/v1/services/routes` — see the bind/firewall notes
above for what makes that reachable in practice. All four builtins are part of the
*native* router only: on `wasm32-unknown-unknown` the server module is compiled out, so
a WASM consumer has the `crateName`/`version`/`echo` boundary and no HTTP surface at
all.

### The playground

What `GET /` actually does (`src/server/playground.js`, no framework, no build step):

- **One automatic request.** At load the page fetches `GET /v1/services/routes` and turns
  every row into a searchable rail (`method`, `path`, `summary`); the search box filters
  all three fields. Nothing else is sent until you press Send, so opening the tool cannot
  call a write endpoint by accident. The first listed entry is loaded into the composer so
  the tool is usable without a click. If that one request fails, the reason stays on
  screen — it survives filter keystrokes — together with a **Try again** button that
  re-runs the request in place, so a restart of the server needs no reload of the page.
- **Method badges are class-coloured, never styled from text.** `get`/`head` read as
  reads, `post` as a write, `put`/`patch` as replacements, `delete` as a removal, and every
  other verb (`options`, `trace`, `connect`, or anything a future route adds) keeps the
  documented neutral badge. The method name only ever becomes a `data-method` attribute,
  so no style value is derived from server text.
- **The composer is derived from the template, never guessed.** Method and path are
  read-only (they always come from the listing), every `{param}` in a path becomes one
  required input, and a body editor appears only for `POST`/`PUT`/`PATCH` — with a starter
  body for this package's own `POST /v1/services/echo` and `{}` for any other
  body-accepting method, so a new endpoint needs no change here. The body is sent with any
  surrounding whitespace trimmed (JSON does not read it, and a pasted newline is otherwise
  invisible). Query parameters and headers are key/value rows: repeated keys survive,
  blank rows are dropped.
- **Invalid input is reported, not sent.** An empty path parameter, an unsupported
  placeholder, a body that is not valid JSON, an illegal header name, a browser-forbidden
  header (`host`, `origin`, `cookie`, …) or a repeated header stops the request and names
  the problem in the composer. A path value is percent-encoded including `/`, `?` and `#`
  (so it cannot change which route is hit — the router decodes it back), and query values
  go through `URLSearchParams`, so a value cannot invent `&`, `#` or another origin.
  Header names that collide with `Object.prototype` members (`constructor`, `toString`,
  `__proto__`) are ordinary headers here: the lookup tables have null prototypes, so
  nothing is refused or called a duplicate because of an inherited property.
- **Requests cannot leave this origin.** `mode: "same-origin"` plus `redirect: "error"`
  (also on the metadata request), `credentials: "same-origin"`, `cache: "no-store"`, and a
  30-second client-side watchdog. **Ctrl+Enter** sends while the composer is idle; while a
  request is in flight Send is locked (requests are not queued) and **Esc** or **Cancel**
  aborts it.
- **Whatever came back is shown.** Status pill, tone-driven from the class stylesheet:
  `ok` for `2xx`, `error` for `4xx`/`5xx`, and `redirect`/`info` for `3xx`/`1xx` — the
  last two are styled and implemented but cannot appear today, because the page fetches
  with `redirect: "error"` (a `3xx` becomes a failed request) and the fetch API never
  exposes a `1xx`. Then the exact URL the page built, the response headers sorted, the
  body pretty-printed when it is JSON and shown verbatim otherwise, plus the wire size and
  the elapsed time. A `4xx`/`5xx` still renders the service's own text, and the three
  no-response states are explicit: cancelled, timeout, and "the request never produced a
  response".
- **Untrusted text stays text.** Every server-supplied string goes in through
  `textContent`/`createElement`, so an echo of `<img src=x onerror=alert(1)>` displays
  those characters; there is no `innerHTML`, no `eval`, no inline script or style, and the
  CSP therefore grants no `'unsafe-inline'` and no second origin.

Drive it without a browser: `npm run check:ui` (hermetic) or
`npm run check:ui -- --base http://127.0.0.1:5001` against a running server — see
[Scripts](#scripts).

### Endpoints as they behave on the wire

The sample routes under `examples/endpoints/**` — each mounted by its
`<resource>-route.rs` and implemented in the `<resource>-core.rs` beside it (see
[How an example endpoint is laid out](#how-an-example-endpoint-is-laid-out)), answered
exactly as listed here:

| Method | Path | Response |
| --- | --- | --- |
| `GET` | `/v1/services/health` | `200 {"service":"typecrate-services","version":"1.0.2"}` |
| `GET` | `/v1/services/echo/{value}` | `200 {"value":"<value>"}` |
| `GET` | `/v1/services/echo/{value}?prefix=pre-` | `200 {"value":"pre-<value>"}` |
| `POST` | `/v1/services/echo` with `{"value":"x"}` | `200 {"value":"x"}` |
| `POST` | `/v1/services/echo` with a truncated JSON body | `400 Failed to parse the request body as JSON: ...` |
| `POST` | `/v1/services/echo` with `{"value":{"a":1}}` or a missing `value` | `422 Failed to deserialize the JSON body ...` |
| `GET` | `/v1/services/locations` | `200 [...]`, every stored location in ascending id order, `200 []` when nothing is stored |
| `GET` | `/v1/services/locations?city=Berlin` | `200 [...]` narrowed to an **exact** `city` match; an absent or empty `city` means no filter, and an unknown query key is tolerated |
| `POST` | `/v1/services/locations` with `{"name":"Flagship store","city":"Berlin","country":"DE"}` | `201 {"id":1,"name":"Flagship store","city":"Berlin","country":"DE"}` plus `location: /v1/services/locations/1` |
| `GET` | `/v1/services/locations/{id}` | `200` the record; `404 {"error":"no location with id 7"}` for an unknown id; `400` when `{id}` is not a number |
| `PUT` | `/v1/services/locations/{id}` with the **whole** record | `200` the replacement; `404` for an unknown id, because `PUT` never creates |
| `PATCH` | `/v1/services/locations/{id}` with `{"city":"Munich"}` or `{}` | `200` the record with only the sent fields changed (`{}` changes nothing, an absent or `null` field keeps its value); `404` for an unknown id |
| `DELETE` | `/v1/services/locations/{id}` | `204` with **no body**; `404` for an unknown id |
| `GET` | `/v1/services/pets` | `200 [...]`, every stored pet in ascending id order, `200 []` when nothing is stored (no query filter) |
| `POST` | `/v1/services/pets` with `{"name":"Rex","species":"dog","age":3}` | `201 {"id":1,"name":"Rex","species":"dog","age":3}` plus `location: /v1/services/pets/1` |
| `GET` | `/v1/services/pets/{id}` | `200` the record; `404 {"error":"no pet with id 7"}` for an unknown id; `400` when `{id}` is not a number |
| `PUT` | `/v1/services/pets/{id}` with the **whole** record | `200` the replacement; `404` for an unknown id, because `PUT` never creates |
| `PATCH` | `/v1/services/pets/{id}` with `{"age":4}` or `{}` | `200` the record with only the sent fields changed (`{}` changes nothing, an absent or `null` field keeps its value); `404` for an unknown id |
| `DELETE` | `/v1/services/pets/{id}` | `204` with **no body**; `404` for an unknown id |
| `HEAD` | any path registered as `GET` (e.g. `/v1/services/health`) | the `GET`'s status and headers with **no body** — axum answers `HEAD` for every `GET` route, which is why a wrong method there reports `405 allow: GET,HEAD`. The listing and the playground still report only the declared `GET` |
| any | a registered path, unsupported method (e.g. `DELETE /v1/services/health`, `OPTIONS` preflight) | `405` |
| any | an unregistered path (also `..%2F` tricks) | `404` |

The two REST samples are the crate's first **writing** endpoints, and they follow one
contract each: `pets` and `locations` behave identically apart from their record fields and
the locations-only `?city=` filter.

- `POST` answers `201` with the created record and a `location` header that points at
  `GET /v1/services/<resource>/{id}` on this same origin. Every field is required —
  `name`/`species`/`age` for a pet, `name`/`city`/`country` for a location.
- A collection `GET` answers the whole array in ascending id order, and `[]` rather than a
  `404` when nothing is stored.
- `PUT` replaces an existing record completely and **never creates** one: an unknown id is
  `404`, not an insert. `PATCH` writes only the fields it is sent, so an absent field or an
  explicit `null` keeps the stored value and `{}` is a valid no-op.
- `DELETE` removes the record and answers `204` with an empty body; an unknown id is `404`.
- Ids are per resource, and the next `POST` always answers `max(stored id) + 1` — an empty
  store therefore starts at `1`. A freed id comes back exactly when the deleted record held the
  **highest** id: after `DELETE /v1/services/pets/1` on a store that only held `1`, the next
  `POST` answers `1` again, while with `1` and `2` stored that same delete leaves `2` highest, so
  the next `POST` answers `3` and the hole at `1` stays (nothing renumbers the survivors).
- `{id}` has to be a number: `/v1/services/pets/abc` is a `400` from the path extractor and
  never reaches the store.

The JSON body rules are the framework's own, shared with `echo`: a request without
`Content-Type: application/json` is a `415` whose body is the axum rejection naming the
media type, a truncated body is a `400 Failed to parse the request body as JSON: ...`, and a
body that does not fit the record — a missing `age`, or `"age":"three"` — is a
`422 Failed to deserialize the JSON body ...`. None of those three ever touches the store,
and the `1 MiB` cap still answers `413` first (see the security notes above).

```bash
curl -i http://127.0.0.1:5001/v1/services/health
curl -i "http://127.0.0.1:5001/v1/services/echo/there?prefix=pre-"   # {"value":"pre-there"}
curl -i -X POST -H 'Content-Type: application/json' \
     --data '{"value":"round-trip"}' http://127.0.0.1:5001/v1/services/echo
```

A multi-byte path segment survives routing and serialisation
(`GET /v1/services/echo/%F0%9F%98%80` → `{"value":"😀"}`), and `%2F` inside a path
parameter is decoded (`/v1/services/echo/a%2Fb` → `{"value":"a/b"}`).

The `pets` sample end to end, i.e. the walkthrough the statuses above describe:

```bash
curl -i -X POST -H 'Content-Type: application/json' \
     --data '{"name":"Rex","species":"dog","age":3}' http://127.0.0.1:5001/v1/services/pets
#   201 Created
#   location: /v1/services/pets/1
#   {"id":1,"name":"Rex","species":"dog","age":3}

curl -s http://127.0.0.1:5001/v1/services/pets/1   # 200 {"id":1,"name":"Rex","species":"dog","age":3}
curl -s http://127.0.0.1:5001/v1/services/pets     # 200 [{"id":1,"name":"Rex","species":"dog","age":3}]
curl -s http://127.0.0.1:5001/v1/services/pets/7   # 404 {"error":"no pet with id 7"}

# PATCH writes only what it is sent: name and species stay exactly as they were.
curl -i -X PATCH -H 'Content-Type: application/json' \
     --data '{"age":4}' http://127.0.0.1:5001/v1/services/pets/1
#   200 {"id":1,"name":"Rex","species":"dog","age":4}

# PUT replaces the record with what it is sent, and it does not create.
curl -i -X PUT -H 'Content-Type: application/json' \
     --data '{"name":"Rex","species":"dog","age":5}' http://127.0.0.1:5001/v1/services/pets/1
#   200 {"id":1,"name":"Rex","species":"dog","age":5}
curl -i -X PUT -H 'Content-Type: application/json' \
     --data '{"name":"Max","species":"cat","age":1}' http://127.0.0.1:5001/v1/services/pets/7
#   404 {"error":"no pet with id 7"}

curl -i -X DELETE http://127.0.0.1:5001/v1/services/pets/1
#   204 No Content, empty body

curl -s http://127.0.0.1:5001/v1/services/pets     # 200 []
curl -i -X POST -H 'Content-Type: application/json' \
     --data '{"name":"Rex","species":"dog","age":3}' http://127.0.0.1:5001/v1/services/pets
#   201 {"id":1,...} - the store is empty again, so `max(stored id) + 1` is 1: the deleted
#   highest id is handed straight back (see the id rule above)
```

The `locations` sample, where the list filter is the part worth pinning:

```bash
curl -i -X POST -H 'Content-Type: application/json' \
     --data '{"name":"Flagship store","city":"Berlin","country":"DE"}' \
     http://127.0.0.1:5001/v1/services/locations
#   201 Created
#   location: /v1/services/locations/1
#   {"id":1,"name":"Flagship store","city":"Berlin","country":"DE"}

curl -i -X POST -H 'Content-Type: application/json' \
     --data '{"name":"Warehouse","city":"Hamburg","country":"DE"}' \
     http://127.0.0.1:5001/v1/services/locations
#   201 Created
#   location: /v1/services/locations/2
#   {"id":2,"name":"Warehouse","city":"Hamburg","country":"DE"}

curl -s "http://127.0.0.1:5001/v1/services/locations"
#   200 [{"id":1,"name":"Flagship store","city":"Berlin","country":"DE"},
#        {"id":2,"name":"Warehouse","city":"Hamburg","country":"DE"}]

curl -s "http://127.0.0.1:5001/v1/services/locations?city=Berlin"
#   200 [{"id":1,"name":"Flagship store","city":"Berlin","country":"DE"}]

# The match is exact, so case matters; an unknown key is tolerated, not an error.
curl -s "http://127.0.0.1:5001/v1/services/locations?city=berlin&lang=en"
#   200 []

curl -s "http://127.0.0.1:5001/v1/services/locations?city="
#   200 the whole list again - an empty value is no filter

curl -i -X PATCH -H 'Content-Type: application/json' \
     --data '{"country":"DEU"}' http://127.0.0.1:5001/v1/services/locations/2
#   200 {"id":2,"name":"Warehouse","city":"Hamburg","country":"DEU"}
```

**Where those records live.** `pets` and `locations` write one plain JSON array each, below
the *data* root and never below the scan root: `<database-root>/pets/pets.json` and
`<database-root>/locations/locations.json`. The root is the one already configured by
`--database-root <dir>` / `TYPECRATE_SERVICES_DATABASE_ROOT` (see the settings table above),
so nothing new is introduced here — only two more folders below it, on the same
"whoever writes first creates its own folder" rule. A folder or file that does not exist yet
reads as the empty store, the first write creates it, and the record set survives a restart
because the file **is** the store: there is no database and no cache to invalidate, and id
numbering after a restart is still `max(stored id) + 1` of the records actually in the file, so
a restart cannot re-number what survived — and an id whose record was deleted is available
again once it was the highest one.

Embedding instead of launching: `typecrate_services::server::app(state)` returns the
exact router the executable serves, so a host can mount it on its own listener —
which is how `tests/server_http.rs` runs every HTTP assertion against a real socket
without duplicating the route table.

```rust
use typecrate_services::server::{AppState, app};
let router = app(AppState::new("D:\\data"));   // axum::Router, ready for axum::serve
```

## Routes are compiled in at build time

There is **no runtime route registry**. `build.rs` scans the tree for `*-route.rs` files
before the crate compiles, writes `$OUT_DIR/service_routes.rs`, and `src/lib.rs` includes
it at crate scope; the generated `register_routes` chains every discovered route file into
the `Router`, and the generated `route_metadata` concatenates the same modules'
`ENDPOINTS` into the endpoint listing. A route that does not type-check is a normal
`rustc` error pointing at that route file — or at the `*-core.rs` file holding its
handler — never a failed import at request time.

Discovery rules (`build_support/`):

- Scan root: the **repository root** by default (this package's directory, three
  levels up). Override with the compile-time variable
  `TYPECRATE_SERVICES_ROUTE_ROOT` — a relative value resolves against this package,
  so `.` means "this package" and `../../..` means the default. An empty value means
  "use the default".
- A route file is **`*-route.rs`**: a non-empty stem followed by the exact lowercase
  `-route` suffix and `.rs` (`pets-route.rs`, `locations-route.rs` — the convention is an
  all-lowercase file name), **inside an `endpoint` or `endpoints` directory** — the same
  directory rule the TypeScript scanner uses, so `src/pets-route.rs` stays a plain module.
  The suffix match is exact, so `pets-Route.rs` and `pets-route.RS` are not route files
  either.
- **Anything that is not `*-route.rs` is ignored** — silently, by name, with no
  diagnostic. That is what keeps the implementation files (`*-core.rs`, see
  [How an example endpoint is laid out](#how-an-example-endpoint-is-laid-out)), `mod.rs`,
  `*.test.rs` / `*.spec.rs` and `*-route.ts` out of the generated table. A file below an
  endpoint tree is compiled only when a route file pulls it in with `#[path]`, never
  because the scanner walked past it.
- The walk is gitignore-aware (`.gitignore`, `.ignore`, `.git/info/exclude` inside
  the tree), skips hidden trees, never follows symlinks, and prunes
  `node_modules`/`target`/`dist`/`pkg`/`.rust`/`.turbo`/`temp`/`tmp`/… by name even
  where no ignore rule exists. Ignore files *above* the scan root and the user's
  global gitignore are **not** consulted, so the generated table does not change
  between machines.
- Registration order is the scanner's deterministic order (byte-wise comparison of
  the scan-root-relative path), and the generated file is rewritten only when its
  bytes change — re-scanning an unchanged tree never triggers a recompile.

**Naming migration: the rule changed from a prefix to a suffix.** The scanner used to
pick up a `service-route*` **prefix** (`service-route.rs`, `service-route-echo.rs` and
any `-suffix` variant). It now picks up only a `-route.rs` **suffix**, and the old
spellings are **silently ignored**: nothing errors and nothing warns, a file that does not
match just drops out of the build, so its routes leave both the router and
`GET /v1/services/routes`. Renaming a route file *away* from a matching name is therefore
the step that removes a route. The one old name that still matches the new rule is the
bare `service-route.rs`, whose stem is `service`: it keeps compiling, and it should still
be renamed to `<resource>-route.rs`, because the stem is the part that says which resource
the file mounts.

### How an example endpoint is laid out

Every example follows one two-file template: a **route file** that describes the route and
nothing else, and a **core file** that holds the implementation.

```text
examples/endpoints/<resource>/
    <resource>-route.rs   # lightweight: module docs + one #[path] mod pulling the core file
                          # + a use of the handler fns + the single crate::service_routes! block
                          # (routes + summaries only; no DTOs, no storage, no handler bodies)
    <resource>-core.rs    # the implementation: DTOs, storage helpers, handler fns.
                          # Compiled only because the route file pulls it in via
                          # #[path = "<resource>-core.rs"] mod handlers;
                          # ignored by the scanner because its name is not *-route.rs.
```

The concrete set today is exactly that shape, one pair per sample resource:

```text
examples/endpoints/echo/echo-route.rs             examples/endpoints/echo/echo-core.rs
examples/endpoints/health/health-route.rs         examples/endpoints/health/health-core.rs
examples/endpoints/locations/locations-route.rs   examples/endpoints/locations/locations-core.rs
examples/endpoints/pets/pets-route.rs             examples/endpoints/pets/pets-core.rs
```

**Why the route file stays that small.** A route file is the unit the framework consumes:
the scanner finds it by name, the generated table names its module, and its single
`crate::service_routes!` block is the one place that produces **both** the mounted router
and the `GET /v1/services/routes` row (see
[Routes are compiled in at build time](#routes-are-compiled-in-at-build-time)). Everything
that pairing needs is a verb, a path literal and a summary — a handful of lines — while
DTOs, storage helpers and handler bodies sit one `#[path]` away instead of bloating the
file the build reads.

**How the core file gets compiled: never by name.** The route file declares
`#[path = "<resource>-core.rs"] mod handlers;`, so `rustc` reads the core file as a module
nested **inside** the route module, and the route file pulls the handler fns into scope
with `use handlers::<fn>;` before the macro block names them. The scanner ignores the core
file because its name is not `*-route.rs`, so one resource is discovered exactly once, with
one generated identifier, no matter how many implementation files lie next to it.

**The rule that keeps this intact: an implementation file must not be named
`*-route.rs`.** A `pets-route-impl.rs` would be discovered as a route module of its own,
and the generated table asks every discovered module for `register` **and** `ENDPOINTS` —
so a plain implementation file fails to compile, and one that did expose them would mount
and list the same resource a second time. Implementation files are `*-core.rs`, and only
the route file carries the macro.

**Adding an endpoint the same way (the two-file workflow).**

1. Create `examples/endpoints/<resource>/<resource>-route.rs` **and**
   `<resource>-core.rs` inside an `endpoint` or `endpoints` directory — the pair, because
   only the route file is ever discovered.
2. Put the DTOs, the storage helpers and the handler fns in the core file as `pub` items,
   and put the `#[path = "<resource>-core.rs"] mod handlers;` plus the `use` lines in the
   route file.
3. Declare every route once in the route file's single `crate::service_routes!` block:
   verb, path literal, handler name, summary text, `;`. A summary belongs to that entry —
   not to the file name and not to the path — so renaming a file never changes a listing
   row.
4. Rebuild: see [Rebuild tracking](#rebuild-tracking-what-re-runs-the-scan-and-what-needs-a-re-scan)
   for what re-runs the scan. The new row then appears in `GET /v1/services/routes` and in
   the playground rail with nothing else touched.

**What the split does not change is the wire surface.** The `*-route.rs` rename and the
two-file layout are purely internal file organisation: the fifteen routes listed in
[Where the server answers](#where-the-server-answers), the listing endpoint itself, every
method, path, status and summary, and every REST rule documented in
[Endpoints as they behave on the wire](#endpoints-as-they-behave-on-the-wire) are identical
before and after. A client cannot tell that `pets` is now two files, and not one `curl`
example in this README changes.

### Rebuild tracking (what re-runs the scan, and what needs a re-scan)

Cargo's only directory watch is **recursive**, and it applies none of the scanner's
prune rules, so watching the scan root would watch the build's own output
(`.rust/target`, set by the repository-root `.cargo/config.toml`) and re-run the scan
on *every* cargo invocation, which recompiles the crate every time (a build-script
re-run recompiles even when the generated bytes are identical). `build.rs` therefore
reports the narrowest set that still covers the changes that matter, and never an
ancestor of `$OUT_DIR`:

| Change | Re-runs the scan | Why |
| --- | --- | --- |
| add / edit / remove a route file inside a discovered `endpoint`/`endpoints` tree | **automatic** | that tree is a watched directory (recursive) and each route file is watched directly |
| add / remove a whole route **subdirectory** inside such a tree | **automatic** | same directory watch |
| create / edit / delete an ignore file **inside** such a tree | **automatic** | same directory watch |
| edit an ignore file that **already existed** above such a tree (including the scan root's own `.gitignore`) | **automatic** | watched as an individual file (non-recursive) |
| delete a discovered endpoint tree | **automatic** | the watched path goes missing, which cargo counts as a change |
| **a brand-new `endpoint`/`endpoints` tree** outside the watched trees | **explicit re-scan** | nothing watched covers the new location, and cargo has no shallow directory watch |
| an ignore file **newly created** above a watched tree | explicit re-scan | only existing paths can be watched |
| a route file's content edit on its own | no re-scan needed | `rustc`'s dep-info for the `#[path]` module recompiles it |
| add / edit / remove a `*-core.rs` implementation file inside such a tree | **automatic**, and it changes nothing | the recursive directory watch sees the file, but a core file is not a route file, so the regenerated table is byte-identical; only `rustc`'s dep-info for the nested `#[path]` module recompiles |

Regenerating the table is the only step: the next `cargo build`/`run-native` compiles
a new route in, and deleting a **route** file takes it back out (deleting only a core
file is a compile error on the route file, never a silent removal). Two ways to trigger
the explicit re-scan, neither of them a `cargo clean` and neither a daemon:

- **npm scripts (automatic):** `build`, `start`, `run-native`, `check` and `test` all
  start with `node scripts/discover-services.mjs` (`npm run discover` on its own) -
  no dependencies, ~50 ms. It walks the tree with the scanner's prune rules and
  rewrites one file, `<repo>/Temp/typecrate-services/route-discovery.json`, which
  `build.rs` watches as an individual path. Its revision hashes **paths only**, so an
  unchanged tree writes nothing and cargo stays `Fresh`; a new endpoint folder (or a
  new ignore file) changes the bytes and the next cargo invocation re-runs the scan.
  The walk is a deliberate superset (gitignore rules are not re-implemented in JS), so
  an extra path can at worst cost one harmless re-scan, never a missed one. `Temp/` is
  disposable: after it is cleared, `build.rs` seeds the file again and the pre-step
  repopulates it, which costs one extra re-scan.
- **bare cargo (manual):** set `TYPECRATE_SERVICES_RESCAN` to any new value
  (`TYPECRATE_SERVICES_RESCAN=<timestamp> cargo check`); cargo tracks that variable,
  and repeating the same value re-runs nothing. `npm run discover -- --force` rewrites
  the snapshot file for the same effect without an environment variable.

The caveat that comes with a recursive directory watch: keep generated output out of
`endpoint`/`endpoints` trees. The scanner prunes `node_modules`/`dist`/`target`/… from
*discovery*, but cargo does not know those rules when it watches a tree, so an artifact
written inside a watched tree does re-run the scan - this repository's
`runtime/endpoint/` is such a tree, since it carries its own `node_modules/`. That is a
one-off re-scan, not a loop: nothing a cargo build writes lives inside an endpoint tree,
which is why two consecutive `cargo check` and `cargo build --release` invocations are
both `Fresh` (0.4 s and 0.3 s after the first build).

The route-file contract — one `crate::service_routes!` invocation, ordinary axum
handlers, ordinary extractors. Both files of the `health` sample, in full for what the
build cares about:

```rust
// examples/endpoints/health/health-route.rs — the lightweight half: nothing but the pull and the routes.
//! GET /v1/services/health — the route this file mounts.
#[path = "health-core.rs"]
mod handlers;

use handlers::health;

// One entry per mounted (method, path) pair, each entry ending in `;`.
crate::service_routes! {
    GET "/v1/services/health" => health, "Reports the service name and version";
}
```

```rust
// examples/endpoints/health/health-core.rs — the implementation half, never discovered by name.
use axum::Json;
use axum::extract::State;

#[derive(serde::Serialize)]
pub struct HealthResponse { /* ... */ }

pub async fn health(State(_state): State<crate::server::AppState>) -> Json<HealthResponse> { /* ... */ }
```

That macro (`src/server/metadata.rs`) is the whole contract, and it is what makes the
listing honest: from one entry it emits **both** items the generated table refers to —
expanded **in the route module**, where the `use handlers::health;` line has already put
the core file's fn in scope, so the emitted body needs no path of its own —

```rust
pub fn register(
    router: axum::Router<crate::server::AppState>,
) -> axum::Router<crate::server::AppState> {
    router.route("/v1/services/health", axum::routing::on(axum::routing::MethodFilter::GET, health))
}

pub const ENDPOINTS: &[crate::server::EndpointMeta] = &[crate::server::EndpointMeta {
    method: "GET", // `stringify!` of the verb above, never a re-typed string
    path: "/v1/services/health",
    summary: "Reports the service name and version",
}];
```

so a verb or a path cannot be renamed in one view and missed in the other: the method is
`stringify!` of the very identifier handed to `MethodFilter` (an unknown verb is a
compile error on the route file), and the path literal is the same token in
`.route(..)` and in `EndpointMeta::path`. Nothing in `build_support/` reads a route
file's *contents*, so the listing is not a parse of the source.

The macro has to be used from **inside this crate** (which is what every discovered route
file is; the core file is only a submodule of it, so the macro never moves there), because
the generated signature names `$crate::server::AppState`; it is `#[macro_export]`ed, so a
route file reaches it as `crate::service_routes!` with no `use` statement. A **route** file
that writes `register` by hand instead is still legal, but then it must also expose
`pub const ENDPOINTS: &[crate::server::EndpointMeta]` with the same entries in the same
order: the generated `route_metadata()` names `<module>::ENDPOINTS` for **every**
discovered module, so a missing const is an ordinary rustc error pointing at the generated
function rather than a listing that quietly omits an endpoint.

An entry repeated with a *different* method is supported (axum merges the two
`MethodRouter`s for that path, and the listing keeps one row per method); the same method
*and* path twice is an axum panic while the router is being built, never a silent
override. An empty invocation is legal too: `register` becomes the identity and
`ENDPOINTS` the empty slice, which is what a placeholder route file compiles to.

Each discovered **route file** becomes `#[path = "<absolute path>"] mod <identifier>;`,
where `<identifier>` is the scan-root-relative path with every non-`[A-Za-z0-9_]`
character replaced by `_` — so `endpoints/echo/echo-route.rs` maps to
`..._endpoints_echo_echo_route` while the **files on disk keep their hyphens**. Only route
files get an identifier: the core file is the `handlers` module nested inside that route
module (`<identifier>::handlers`), so it never shows up in the generated table on its own.
Identifiers are generated; nothing should hardcode them, only the generated
`register_routes` and `route_metadata` refer to them (`<identifier>::register` and
`<identifier>::ENDPOINTS`).
Collisions (`a-b/` vs `a_b/`) get deterministic `_2`/`_3` suffixes. A digit-leading
or capitalised segment (`endpoints/2024/`, `endpoints/Unicode/`) sanitises into an
identifier that is a valid name but not snake case, so the generator emits
`#[allow(non_snake_case)]` in front of exactly those declarations (rustc rejects a
capital or a doubled separator, measured on 1.99.1): the generated file is `include!`d
into the crate, so `npm run lint:strict` lints it like hand-written code, and the
attribute is per-declaration to keep the full lint over every clean route file.

Any extra crate a route file needs (an extractor, a template engine, a client) — or the
core file it pulls in needs — is a **dependency of this package's `Cargo.toml`**, under the
`cfg(not(all(target_arch = "wasm32", target_os = "unknown")))` target — route files
compile into the native crate, and a dependency outside that gate also has to build
for `wasm32-unknown-unknown`.

## Scripts

| Script | Command | Gate |
| --- | --- | --- |
| `discover` | `node scripts/discover-services.mjs` | native: rewrites the watched discovery snapshot (`--force`, `--print`) || `build` | `node scripts/discover-services.mjs && cargo build --release --locked` | native: rlib + cdylib + the server executable |
| `start`, `run-native` | `node scripts/discover-services.mjs && cargo run --bin typecrate-services-server --locked --` | native: forwards everything after `npm … --` |
| `check` | `node scripts/discover-services.mjs && cargo check --workspace --all-targets --locked` | native |
| `check:ui` | `node scripts/check-playground.mjs` | **optional**, JS: drives the embedded page in jsdom (the workspace root's copy; `--base <origin>` runs the same suite against a live server). Not part of `verify`, which stays JS-free |
| `test` | `node scripts/discover-services.mjs && cargo test --workspace --locked` | native: unit + discovery + real-socket HTTP |
| `lint` | `cargo clippy --workspace --all-targets --locked` | what root `turbo run test` executes; must not deny warnings |
| `lint:strict` | the same with `-D warnings` | CI/verify gate (it lints the generated module too) |
| `fmt`, `fmt:check` | `cargo fmt --all [-- --check]` | native |
| `verify` | `fmt:check` + `lint:strict` + `test` + `build` | **native gate** |
| `wasm:check` | `cargo check --target wasm32-unknown-unknown --all-targets --locked` | WASM cross-compile |
| `build:wasm` | `node scripts/build-wasm.mjs` | WASM: artifact + glue + declarations |
| `smoke:wasm` | `node examples/node-smoke.mjs` | WASM: runs `pkg/` in Node, exact values |
| `verify:wasm` | `wasm:check` + `build:wasm` + `smoke:wasm` | **WASM chain**, secondary |
| `prepack` | `npm run build:wasm` | `npm pack`/`publish` cannot ship dead entry paths |

Every cargo script runs `--locked` against the committed `Cargo.lock` (this
workspace has its own lock; the root workspace keeps a separate one), so a build
never re-resolves dependencies — which also matters for the WASM path, because the
`wasm-bindgen` CLI has to match the version recorded in that lock. If the lock is
missing or predates a manifest change, cargo stops and you regenerate on purpose
with `cargo generate-lockfile`.

## WASM (secondary)

The WASM module is the published npm surface today: `npm pack` ships `pkg/` (glue +
declarations + `.wasm`) plus `scripts/`, `examples/` and this README, but **no
`src/` and no `Cargo.toml`** — the native server is a repository-level artifact,
built from this directory. The same `src/lib.rs`
compiles for both: the server module, the generated route table and the
axum/tokio stack are gated to non-wasm targets, and `src/bin/server.rs` carries an
empty `main` for the wasm target so `build:wasm` still cross-compiles.

Prerequisites — the pipeline builds nothing by itself and installs nothing:

1. Rust 1.99.1 with the wasm target: `rustup toolchain install` (reads
   `rust-toolchain.toml`).
2. The `wasm-bindgen` **CLI**, at exactly the version `Cargo.lock` pins the
   `wasm-bindgen` library at (currently `0.2.130`). It is not a cargo subcommand
   and rustup does not manage it:

   ```bash
   cargo install wasm-bindgen-cli --locked --version 0.2.130
   ```

   or unpack the matching prebuilt release into `.rust/bin` (`rust/README.md`'s
   local tool home). An arbitrary location works too: set
   `TYPECRATE_WASM_BINDGEN=/path/to/wasm-bindgen`. `scripts/build-wasm.mjs` reads
   the pin out of `Cargo.lock` and refuses a CLI whose version differs, instead of
   letting the generated loader panic later.

```bash
yarn workspace @typecrate/services wasm:check
yarn workspace @typecrate/services build:wasm
yarn workspace @typecrate/services smoke:wasm
```

`TYPECRATE_WASM_DEBUG=1` in front of `build:wasm` builds the unoptimised artifact
instead of the release one (cargo has no `--debug` flag: the debug profile is the
default, so the script simply omits `--release`).

`build:wasm` writes `pkg/typecrate_services.js` (ESM glue),
`pkg/typecrate_services.d.ts` (generated declarations) and
`pkg/typecrate_services_bg.wasm`, and fails if any of them is missing or does not
export/declare `crateName`, `version` and `echo`. Because `pkg/` is git-ignored, a
fresh clone has to run `build:wasm` before the JS entry points resolve.

The generated entry's **default export is the init function** (`__wbg_init` in the
`0.2.130` output this lock pins). With no argument it loads the sibling
`typecrate_services_bg.wasm` through its own module URL; with an argument it accepts
a `URL`, a `Request`, a `Response`/`WebAssembly.Module`/bytes, or a
`{ module_or_path }` object — passing the value directly instead of in an object
prints `using deprecated parameters for the initialization function` (the shipped
`examples/node-smoke.mjs` still does that, so `smoke:wasm` prints the notice; the
object form is the one to copy).

`--target web` is deliberate (see `rust/README.md` "One artifact, both runtimes"):
one module for browser **and** Node, no bundler plugins, no CommonJS-only build,
and no `wasm-pack` (unmaintained by the wasm-bindgen team, and it would overwrite
this hand-authored `package.json`).

### Browser

```ts
import init, { echo, version } from "@typecrate/services";

// No argument: the glue locates `typecrate_services_bg.wasm` relative to its own
// module URL, which is what keeps one artifact working for browser and Node.
await init();

version(); // "1.0.2"
echo("identity"); // "identity"
```

Pass an argument only when the host deliberately places the asset somewhere the
module URL cannot resolve (own CDN path, prefetched `Response`,
`WebAssembly.Module`).

### Node

Node's `fetch` cannot read `file:` URLs, so pass the bytes
(`examples/node-smoke.mjs` is the runnable version of this):

```js
import { readFileSync } from "node:fs";
import init, { echo } from "@typecrate/services";
await init({ module_or_path: readFileSync("./node_modules/@typecrate/services/pkg/typecrate_services_bg.wasm") });
echo("identity"); // "identity"
```

### Module format: ESM only

The package is `"type": "module"` and `exports` publishes a single `import`
condition (`./pkg/typecrate_services.js`, typed by `./pkg/typecrate_services.d.ts`)
with no `require` condition and no CJS fallback — wasm-bindgen `--target web`
emits ES modules. A CommonJS host therefore cannot `require("@typecrate/services")`;
it has to go through `await import("@typecrate/services")`. There is also no
`development`/source condition: the old Vite-era `index.ts` entry is gone, so every
consumer resolves the generated artifact (run `build:wasm` first).

The wasm build carries `-C target-feature=+simd128` from the root cargo config, so
the support floor is Chrome 91+ / Firefox 90+ / Safari 16+ / Node 16+.

## Scope and parity gaps

This milestone is the framework, not the migration:

- **No TypeScript endpoint has been ported.** The fifteen documented routes are the
  package's own samples (`examples/endpoints/**`), and the two REST samples are toy CRUD
  written to demonstrate the framework, not a port of anything: the `@underload/service`
  catalogue (comfy, harness, database, preferences, …) still runs on the TS
  gateway, which stays untouched.
- **Single port, single origin.** No multi-port gateway, no static-file mount or web
  root (the playground page at `/` is embedded in the binary, not read from disk), no
  streaming or websocket endpoints (`ws` is not even an enabled axum feature), and
  no media/range serving.
- **No security middleware yet**: no bearer/API-key auth, no Origin/CORS gate
  (an `OPTIONS` preflight is a plain `405`), no rate limiting. Because the default
  bind is `0.0.0.0`, "anything on the LAN can call this server" is the honest
  description of the current posture: the body cap and the no-filesystem-path rule
  are what the code enforces, while *trusted network* is an operating assumption the
  operator keeps — `--addr 127.0.0.1` narrows it to this machine when that
  assumption is not acceptable. Neither is a substitute for a proxy with auth.
- **Routes are build-time.** A new route file needs a rebuild; nothing is registered
  on a hot reload. The automatic watch set is deliberately narrow - the endpoint trees
  that produced routes, their route files, and the ignore files above them - because
  cargo's only directory watch is recursive (see
  [Rebuild tracking](#rebuild-tracking-what-re-runs-the-scan-and-what-needs-a-re-scan)).
  A **brand-new endpoint folder** therefore needs one explicit re-scan: automatic for
  the npm scripts (discovery snapshot), manual for bare cargo
  (`TYPECRATE_SERVICES_RESCAN`). This is a bounded trade-off, not full repository-wide
  directory tracking, and it is what keeps consecutive `cargo check`/`build`
  invocations `Fresh` instead of rebuilding on every call.
- **`1 MiB` request bodies, JSON only.** No multipart or streaming handlers (the
  axum features enabled here are `json` + `query`, and `ws` is off), no compression
  layer, and HTTP/1.1 only — axum's default serve stack.
- **WASM does not serve.** On `wasm32-unknown-unknown` the server module and the
  generated table are compiled out, so the module keeps the
  `crateName`/`version`/`echo` boundary and its `main` is an empty stub.

Known, accepted noise on the WASM path today: `wasm-bindgen` reports
`the target_features custom section is missing, likely due to strip = true` (the
release profile strips symbols for size, so the CLI cannot record which Wasm
features the module needs), and `smoke:wasm` prints the init-deprecation notice
above. Neither affects the native milestone.

## Notes

- `Cargo.lock` belongs to this standalone workspace and is committed; the root
  workspace keeps its own. Every cargo script and `scripts/build-wasm.mjs` pass
  `--locked`, so the graph (and the `wasm-bindgen` version the CLI must match) only
  changes when someone runs `cargo generate-lockfile` on purpose.
- Turbo (root `turbo.json`) caches `build` outputs as `dist/**`; this package's
  outputs are `pkg/**` and `.rust/target`, so turbo runs the scripts but does not
  cache the Rust artifacts (cargo does that). The same is true of
  `packages/underload/nexus`.
- The old TypeScript/Vite scaffold (`index.ts`, `vite.config.ts`, `tsconfig*.json`,
  `dist/`) was removed in favour of the Rust crate; nothing outside this package
  imported it. The TypeScript service implementation this crate is meant to reach
  parity with lives in `packages/underload/service`.
