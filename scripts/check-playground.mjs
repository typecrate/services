// Browser-surface check for the embedded playground (the UI half of R2/R3).
//
//     node scripts/check-playground.mjs                        # hermetic: the real
//                                                              # assets, transport = the
//                                                              # fixtures below
//     node scripts/check-playground.mjs --base http://127.0.0.1:5001
//                                                              # the same flow against a
//                                                              # running server
//
// # Why this file exists
//
// `src/server/playground.js` is ~1000 lines of browser code that no cargo test can
// execute. `tests/server_http.rs` proves the ROUTES answer; this file proves the PAGE:
// the listing becomes a clickable rail, the composer turns a template plus user input
// into the exact URL / body / headers that go on the wire, invalid input is reported
// instead of sent, and every failure state (4xx, no response, cancel, timeout, a listing
// that failed to load) stays readable. It is deliberately NOT part of `verify`: the
// native gate stays JS-free, and this is the optional browser-side net.
//
// # How it runs without a browser
//
// jsdom (the workspace root's copy - this package installs nothing) builds the real
// `playground.html` DOM, the real `playground.js` text is evaluated inside it, and only
// the transport is replaced. Hermetic mode answers with `fixtureResponse`, whose bodies,
// statuses and rejection texts are the exact bytes this crate's own endpoints produce
// (same contract `tests/server_http.rs` pins), so the suite is reproducible with no
// server and no network. `--base` runs the identical suite against a live server and
// additionally proves the served asset bytes equal the files in `src/server/` (i.e. the
// running binary embeds the source being tested).
//
// # Determinism rules used here
//
// Every expectation is an exact value. Three things a real socket legitimately varies are
// read from the transport instead of hardcoded: the HTTP reason phrase (`statusLabel`),
// the response header set (`directRequest`), and the elapsed-time string (pattern-matched
// as `<n> ms`, never asserted as a number).
//
// # Exit codes
//
// 0 - all checks passed, or jsdom is unresolvable (a loud skip: the published package
//     installs no dev tooling, and `verify` must not need one);
// 1 - at least one check failed;
// 2 - bad usage, or `--base` points at a server that is not answering.

import { readFile } from 'node:fs/promises';

// ---------------------------------------------------------------------- arguments

const args = process.argv.slice(2);
if (args.includes('--help') || args.includes('-h')) {
    console.log(
        'Usage: node scripts/check-playground.mjs [--base <origin>]\n' +
            '  --base <origin>  drive a running server instead of the hermetic fixtures\n',
    );
    process.exit(0);
}
const baseIndex = args.indexOf('--base');
const BASE = baseIndex === -1 ? null : args[baseIndex + 1];
if (baseIndex !== -1 && !BASE) {
    console.log('--base needs an origin, e.g. --base http://127.0.0.1:5001');
    process.exit(2);
}
if (BASE && !/^https?:\/\/[^/]+\/?$/.test(BASE)) {
    console.log(`--base must be an origin without a path, got ${BASE}`);
    process.exit(2);
}

let jsdomModule;
try {
    jsdomModule = await import('jsdom');
} catch (error) {
    console.log(`SKIPPED: jsdom is not resolvable from this package (${error.code || error.message}).`);
    console.log('SKIPPED: run the check inside the repository, where the workspace root provides jsdom.');
    process.exit(0);
}
const { JSDOM, VirtualConsole } = jsdomModule;

// ------------------------------------------------------------------------ fixtures

/// The eighteen endpoints the compiled server lists, exactly as `GET /v1/services/routes`
/// serialises them (`EndpointMeta` field order is JSON key order, `src/server/metadata.rs`).
///
/// The three `/v1/harness` rows come FIRST: the bridge's route file lives in the harness
/// package (`packages/typecrate/harness/endpoint/harness/harness-route.rs`), and a scan rooted
/// at the repository default sorts `packages/typecrate/h…` before `packages/typecrate/s…`
/// (R2, pinned by `tests/discovery.rs::the_harness_bridge_in_another_package_is_discovered_and_registered`
/// and by `tests/server_http.rs::route_table`). `ANY` is the listing token of a verb-agnostic
/// mount, not a wire verb. The rest is the scanner's byte-wise order of this package's route
/// files, so `POST /v1/services/echo` leads and the two REST sample trees come last,
/// `locations` before `pets`; inside a route file the order is the `crate::service_routes!`
/// entry order.
const LISTING_JSON =
    '[{"method":"ANY","path":"/v1/harness","summary":"Underload harness service document"},' +
    '{"method":"ANY","path":"/v1/harness/stream","summary":"Underload harness realtime stream (harness.v1 WebSocket)"},' +
    '{"method":"ANY","path":"/v1/harness/{*rest}","summary":"Underload harness API resources"},' +
    '{"method":"POST","path":"/v1/services/echo","summary":"Echoes the posted `value` unchanged"},' +
    '{"method":"GET","path":"/v1/services/echo/{value}","summary":"Echoes `{value}`, with `?prefix=` applied"},' +
    '{"method":"GET","path":"/v1/services/health","summary":"Reports the service name and version"},' +
    '{"method":"GET","path":"/v1/services/locations","summary":"Lists every stored location, `?city=` narrows by exact city"},' +
    '{"method":"POST","path":"/v1/services/locations","summary":"Creates a location and answers 201 with it"},' +
    '{"method":"GET","path":"/v1/services/locations/{id}","summary":"Returns one location by id, or 404 when the id is unknown"},' +
    '{"method":"PUT","path":"/v1/services/locations/{id}","summary":"Replaces one location by id, or 404 when the id is unknown"},' +
    '{"method":"PATCH","path":"/v1/services/locations/{id}","summary":"Updates only the sent fields of one location, or 404 when the id is unknown"},' +
    '{"method":"DELETE","path":"/v1/services/locations/{id}","summary":"Removes one location by id and answers 204, or 404 when the id is unknown"},' +
    '{"method":"GET","path":"/v1/services/pets","summary":"Lists every stored pet"},' +
    '{"method":"POST","path":"/v1/services/pets","summary":"Creates a pet and answers 201 with it"},' +
    '{"method":"GET","path":"/v1/services/pets/{id}","summary":"Returns one pet by id, or 404 when the id is unknown"},' +
    '{"method":"PUT","path":"/v1/services/pets/{id}","summary":"Replaces one pet by id, or 404 when the id is unknown"},' +
    '{"method":"PATCH","path":"/v1/services/pets/{id}","summary":"Updates only the sent fields of one pet, or 404 when the id is unknown"},' +
    '{"method":"DELETE","path":"/v1/services/pets/{id}","summary":"Removes one pet by id and answers 204, or 404 when the id is unknown"}]';

/// `{"service","version"}` is pinned by `tests/server_http.rs` and by `src/lib.rs`'s
/// crate==npm version assertion; it is repeated here as a literal because a JS fixture
/// cannot read Cargo metadata.
const HEALTH_JSON = '{"service":"typecrate-services","version":"1.0.2"}';

/// axum's rejection texts for the sample echo contract, measured against a running server
/// (`Json` rejections answer `text/plain`, which is why the page shows them verbatim).
const REJECT_UNSUPPORTED_MEDIA = 'Expected request with `Content-Type: application/json`';
const REJECT_MAP_VALUE =
    'Failed to deserialize the JSON body into the target type: value: invalid type: map, expected a string at line 1 column 9';

/// The hermetic transport: the same answers the compiled-in handlers give.
///
/// `Path<String>` hands the handler the percent-DECODED segment, so a value the page
/// encoded as `a%2Fb%3Fc%23d` arrives as `a/b?c#d` (README "Endpoints as they behave on
/// the wire"); the fixture decodes the same way and applies `?prefix=` exactly like
/// `examples/endpoints/echo/echo-core.rs` (the implementation file the lightweight
/// `echo-route.rs` pulls in via `#[path]`).
function fixtureResponse(method, url, init) {
    const [rawPath, rawQuery] = url.split('?');
    const query = new URLSearchParams(rawQuery || '');
    const path = rawPath.replace(/\/$/, '');
    const json = (body) => [200, { 'content-type': 'application/json' }, body];

    if (method === 'GET' && path === '/v1/services/routes') {
        return json(LISTING_JSON);
    }
    if (path === '/v1/services/health') {
        return method === 'GET' ? json(HEALTH_JSON) : [405, { allow: 'GET,HEAD' }, ''];
    }
    if (path === '/v1/services/echo') {
        if (method !== 'POST') {
            return [405, { allow: 'POST' }, ''];
        }
        const body = init.body === undefined || init.body === null ? '' : String(init.body);
        const contentType = String(init.headers['content-type'] || '');
        if (!contentType.includes('json')) {
            return [415, { 'content-type': 'text/plain; charset=utf-8' }, REJECT_UNSUPPORTED_MEDIA];
        }
        let parsed;
        try {
            parsed = JSON.parse(body);
        } catch {
            parsed = null;
        }
        if (!parsed || typeof parsed.value !== 'string') {
            return [422, { 'content-type': 'text/plain; charset=utf-8' }, REJECT_MAP_VALUE];
        }
        return json(`{"value":${JSON.stringify(parsed.value)}}`);
    }
    if (method === 'GET' && path.startsWith('/v1/services/echo/')) {
        const value = (query.get('prefix') || '') + decodeURIComponent(path.slice('/v1/services/echo/'.length));
        return json(`{"value":${JSON.stringify(value)}}`);
    }
    return [404, {}, ''];
}

// --------------------------------------------------------------------- check runner

const results = [];
let failures = 0;

/** Exact-value assertion: a mismatch prints got/want and fails the run. */
function check(label, actual, expected) {
    const ok = actual === expected;
    results.push(label);
    if (!ok) {
        failures += 1;
        console.log(`FAIL ${label}\n  got:  ${JSON.stringify(actual)}\n  want: ${JSON.stringify(expected)}`);
    }
}

/** Pattern assertion, used only for the value time genuinely varies (elapsed ms). */
function checkPattern(label, actual, pattern) {
    const ok = pattern.test(actual);
    results.push(label);
    if (!ok) {
        failures += 1;
        console.log(`FAIL ${label}\n  got:  ${JSON.stringify(actual)}\n  want: ${pattern}`);
    }
}

// ------------------------------------------------------------------- asset loading

const ASSETS = {
    '/': 'playground.html',
    '/playground.css': 'playground.css',
    '/playground.js': 'playground.js',
};

/** Loads the three assets, and with `--base` also captures what the server sends. */
async function loadAssets() {
    const onDisk = {};
    for (const name of Object.keys(ASSETS)) {
        onDisk[name] = await readFile(new URL(`../src/server/${ASSETS[name]}`, import.meta.url), 'utf8');
    }
    if (!BASE) {
        return { onDisk, served: onDisk, responses: {} };
    }
    const served = {};
    const responses = {};
    for (const name of Object.keys(ASSETS)) {
        const response = await fetch(new URL(name, BASE));
        responses[name] = { status: response.status, headers: Object.fromEntries(response.headers) };
        served[name] = await response.text();
    }
    return { onDisk, served, responses };
}

let assets;
try {
    assets = await loadAssets();
} catch (error) {
    console.log(BASE ? `Cannot read the assets from ${BASE}: ${error.message}\nStart the server (npm start), or drop --base to run hermetically.` : `Cannot read the assets in src/server: ${error.message}`);
    process.exit(2);
}

// Live mode is only evidence if the binary serves the very files this check drives.
if (BASE) {
    for (const name of Object.keys(ASSETS)) {
        check(`GET ${name} status`, assets.responses[name].status, 200);
        check(`served ${name} equals src/server/${ASSETS[name]}`, assets.served[name], assets.onDisk[name]);
    }
    check(
        'GET / content-type',
        assets.responses['/'].headers['content-type'],
        'text/html; charset=utf-8',
    );
    check('GET / cache-control', assets.responses['/'].headers['cache-control'], 'no-store');
    check('GET / x-content-type-options', assets.responses['/'].headers['x-content-type-options'], 'nosniff');
    check('GET / referrer-policy', assets.responses['/'].headers['referrer-policy'], 'no-referrer');
    check('GET /playground.css content-type', assets.responses['/playground.css'].headers['content-type'], 'text/css; charset=utf-8');
    check('GET /playground.js content-type', assets.responses['/playground.js'].headers['content-type'], 'text/javascript; charset=utf-8');
    check(
        'GET / content-security-policy',
        assets.responses['/'].headers['content-security-policy'],
        "default-src 'self'; img-src 'self' data:; script-src 'self'; style-src 'self'; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'",
    );
}
check(
    'the document references only its own two assets',
    assets.onDisk['/'].includes('<link rel="stylesheet" href="/playground.css">') &&
        assets.onDisk['/'].includes('<script src="/playground.js"></script>'),
    true,
);

// ---------------------------------------------------------------- jsdom scaffolding

const consoleErrors = [];
const virtualConsole = new VirtualConsole();
virtualConsole.on('jsdomError', (error) => consoleErrors.push(`jsdomError ${error && error.message}`));
virtualConsole.on('error', (message) => consoleErrors.push(`console.error ${message}`));

/**
 * Installs the recording transport in a jsdom window.
 *
 * The record (method, exact URL string, headers, body, fetch init) IS the evidence: the
 * page builds a URL string, so asserting that string is what pins the composer. The call
 * forwarded to a real `fetch` carries only what Node's fetch understands, so no
 * browser-only init value can turn a live request into a TypeError.
 */
function instrument(window, handler) {
    const calls = [];
    const define = (name, implementation) =>
        Object.defineProperty(window, name, { value: implementation, configurable: true, writable: true });
    window.fetch = (input, init = {}) => {
        const url = typeof input === 'string' ? input : input.url;
        const headers = init.headers ? Object.fromEntries(new globalThis.Headers(init.headers)) : {};
        const call = {
            method: init.method || 'GET',
            url,
            headers,
            body: init.body === undefined ? null : init.body,
            mode: init.mode,
            redirect: init.redirect,
            credentials: init.credentials,
            cache: init.cache,
        };
        calls.push(call);
        if (handler) {
            return handler(call, init);
        }
        if (BASE) {
            return globalThis.fetch(new globalThis.URL(url, BASE), {
                method: init.method,
                headers,
                body: init.body,
                signal: init.signal,
            });
        }
        const [status, responseHeaders, body] = fixtureResponse(call.method, url, { body: init.body, headers });
        return Promise.resolve(new globalThis.Response(body, { status, headers: responseHeaders }));
    };
    // jsdom implements neither fetch nor its classes; the page constructs `new Headers()`
    // and `new AbortController()` on its own window, so those names have to exist there.
    define('Headers', globalThis.Headers);
    define('AbortController', globalThis.AbortController);
    define('AbortSignal', globalThis.AbortSignal);
    return calls;
}

/** One request straight through the transport, for header-set/size comparisons. */
async function directRequest(method, url, init = {}) {
    if (!BASE) {
        const [status, headers, body] = fixtureResponse(method, url, { body: init.body ?? '', headers: init.headers || {} });
        return { status, statusText: '', headers: Object.fromEntries(new globalThis.Headers(headers)), body };
    }
    const response = await globalThis.fetch(new URL(url, BASE), { method, ...init });
    return { status: response.status, statusText: response.statusText, headers: Object.fromEntries(response.headers), body: await response.text() };
}

/**
 * The exact status-pill text for a status, derived from the transport's own answer:
 * the page renders `status` plus `statusText` only when the environment reports a reason
 * phrase (a constructed `Response` never does, a live HTTP/1.1 answer usually does).
 */
async function statusLabel(status, method, url, init) {
    const direct = await directRequest(method, url, init);
    if (direct.status !== status) {
        throw new Error(`the transport answered ${direct.status}, not ${status}, for ${method} ${url}`);
    }
    return String(status) + (direct.statusText ? ` ${direct.statusText}` : '');
}

const dom = new JSDOM(assets.served['/'], {
    url: BASE || 'http://127.0.0.1:5001/',
    contentType: 'text/html',
    runScripts: 'outside-only',
    pretendToBeVisual: true,
    virtualConsole,
});
const { window } = dom;
const requests = instrument(window);
window.eval(assets.served['/playground.js']);

const doc = window.document;
const element = (id) => doc.getElementById(id);
const text = (id) => element(id).textContent;
const value = (id) => element(id).value;
const tone = (id) => element(id).getAttribute('data-tone');
const submit = () => element('request-form').dispatchEvent(new window.Event('submit', { bubbles: true, cancelable: true }));
const rowFor = (ariaLabel) =>
    [...doc.querySelectorAll('#endpoint-list button')].find((button) => button.getAttribute('aria-label') === ariaLabel);
const paramInput = () => doc.querySelector('#path-param-rows .param-row input');
const firstRowKey = (container) => doc.querySelector(`${container} .kv-row .kv-key`);
const firstRowValue = (container) => doc.querySelector(`${container} .kv-row .kv-value`);

/** Polls the DOM until `predicate` is truthy (fixed 25 ms steps, bounded). */
async function waitFor(label, predicate, limit = 200) {
    for (let attempt = 0; attempt < limit; attempt += 1) {
        const found = predicate();
        if (found) {
            return found;
        }
        await new Promise((resolve) => setTimeout(resolve, 25));
    }
    throw new Error(`timed out waiting for ${label}`);
}

const LABEL_OK = await statusLabel(200, 'GET', '/v1/services/echo/probe?prefix=pre-');
const LABEL_422 = await statusLabel(422, 'POST', '/v1/services/echo', {
    body: '{"value":{"a":1}}',
    headers: { 'content-type': 'application/json' },
});
const LABEL_415 = await statusLabel(415, 'POST', '/v1/services/echo', { headers: {} });

// --- 1. the listing drives the rail: rows, order, count, and one automatic request --
await waitFor('endpoint list', () => doc.querySelectorAll('#endpoint-list button').length === 18);
check(
    'endpoint rows',
    [...doc.querySelectorAll('#endpoint-list button')]
        .map((button) =>
            [
                button.querySelector('.badge').textContent,
                button.querySelector('.endpoint-path').textContent,
                button.querySelector('.endpoint-summary').textContent,
            ].join('|'),
        )
        .join('\n'),
    [
        'ANY|/v1/harness|Underload harness service document',
        'ANY|/v1/harness/stream|Underload harness realtime stream (harness.v1 WebSocket)',
        'ANY|/v1/harness/{*rest}|Underload harness API resources',
        'POST|/v1/services/echo|Echoes the posted `value` unchanged',
        'GET|/v1/services/echo/{value}|Echoes `{value}`, with `?prefix=` applied',
        'GET|/v1/services/health|Reports the service name and version',
        'GET|/v1/services/locations|Lists every stored location, `?city=` narrows by exact city',
        'POST|/v1/services/locations|Creates a location and answers 201 with it',
        'GET|/v1/services/locations/{id}|Returns one location by id, or 404 when the id is unknown',
        'PUT|/v1/services/locations/{id}|Replaces one location by id, or 404 when the id is unknown',
        'PATCH|/v1/services/locations/{id}|Updates only the sent fields of one location, or 404 when the id is unknown',
        'DELETE|/v1/services/locations/{id}|Removes one location by id and answers 204, or 404 when the id is unknown',
        'GET|/v1/services/pets|Lists every stored pet',
        'POST|/v1/services/pets|Creates a pet and answers 201 with it',
        'GET|/v1/services/pets/{id}|Returns one pet by id, or 404 when the id is unknown',
        'PUT|/v1/services/pets/{id}|Replaces one pet by id, or 404 when the id is unknown',
        'PATCH|/v1/services/pets/{id}|Updates only the sent fields of one pet, or 404 when the id is unknown',
        'DELETE|/v1/services/pets/{id}|Removes one pet by id and answers 204, or 404 when the id is unknown',
    ].join('\n'),
);
check('endpoint count', text('endpoint-count'), '18 endpoints');
check('endpoint rail state is quiet', text('endpoint-state'), '');
check('the method becomes a data attribute, never a style', doc.querySelector('#endpoint-list .badge').getAttribute('data-method'), 'any');
check('listing request url', requests[0].url, '/v1/services/routes');
check('listing request method', requests[0].method, 'GET');
check(
    'listing request init',
    `${requests[0].mode} ${requests[0].redirect} ${requests[0].credentials} ${requests[0].cache}`,
    'same-origin error same-origin no-store',
);
check('the listing is the only automatic request', requests.length, 1);
check('nothing has been answered yet', text('fact-url'), '');

// --- 2. the first entry is pre-loaded, so the tool works without a click -----------
// The first row is the `/v1/harness` bridge, because the harness route file lives in the harness
// package and a repository-root scan sorts it first (T6/R2) - the playground pre-loads whatever
// the listing puts first, so the expectations follow the listing, not the sample endpoints.
check('auto-selected entry', text('composer-target'), 'ANY /v1/harness');
check('auto-selected method', value('request-method'), 'ANY');
check('auto-selected path', value('request-path'), '/v1/harness');
// `ANY` is a verb-agnostic mount: it is not POST/PUT/PATCH, so the body editor stays hidden.
check('auto-selected starter body', value('request-body'), '');
check('auto-selected row is marked', rowFor('Use ANY /v1/harness - Underload harness service document').getAttribute('aria-current'), 'true');

// --- 3. selection rebuilds the composer: required path params, no body editor ------
rowFor('Use GET /v1/services/echo/{value} - Echoes `{value}`, with `?prefix=` applied').click();
check('composer target', text('composer-target'), 'GET /v1/services/echo/{value}');
check('composer method', value('request-method'), 'GET');
check('composer path', value('request-path'), '/v1/services/echo/{value}');
check('composer summary', text('request-summary'), 'Echoes `{value}`, with `?prefix=` applied');
check('one required input per parameter', doc.querySelectorAll('#path-param-rows .param-row').length, 1);
check('the parameter row names itself', doc.querySelector('#path-param-rows .param-row').getAttribute('data-param'), 'value');
check('the parameter label', doc.querySelector('#path-param-rows label').textContent, 'value');
check('the parameter placeholder', paramInput().placeholder, 'value for {value}');
check('the parameter input is required', paramInput().required, true);
check('a method without a body hides the editor', element('body-group').hidden, true);
check('exactly one row is marked as current', doc.querySelectorAll('#endpoint-list [aria-current]').length, 1);

// --- 4. real dispatch: the composer's URL is the request that goes out -------------
paramInput().value = 'probe';
firstRowKey('#query-rows').value = 'prefix';
firstRowValue('#query-rows').value = 'pre-';
submit();
await waitFor('response body', () => text('response-body') !== '');
check('composed url', requests[requests.length - 1].url, '/v1/services/echo/probe?prefix=pre-');
check('composed method', requests[requests.length - 1].method, 'GET');
check('a GET is sent with no body', requests[requests.length - 1].body, null);
check('a GET is sent with no content-type', requests[requests.length - 1].headers['content-type'], undefined);
check('the request cannot leave this origin', `${requests[requests.length - 1].mode} ${requests[requests.length - 1].redirect}`, 'same-origin error');
check('status pill', text('status-pill'), LABEL_OK);
check('status pill tone', tone('status-pill'), 'ok');
check('response url fact', text('fact-url'), 'GET /v1/services/echo/probe?prefix=pre-');
check('response body is pretty-printed JSON', text('response-body'), '{\n  "value": "pre-probe"\n}');
check('a 2xx leaves the state line empty', text('response-state'), '');
checkPattern('response timing is reported', text('response-timing'), /^\d+ ms$/);
// The rendered header list is compared with the same request made directly, so this pins
// the rendering (and the builtin header set in live mode) without hardcoding `date`.
const echoDirect = await directRequest('GET', '/v1/services/echo/probe?prefix=pre-');
check(
    'response headers are listed, sorted',
    [...doc.querySelectorAll('#response-headers dt')].map((node) => node.textContent).join(','),
    Object.keys(echoDirect.headers)
        .sort()
        .map((key) => `${key}:`)
        .join(','),
);
check(
    'body size fact is the wire size',
    text('body-size'),
    `${new TextEncoder().encode(echoDirect.body).byteLength} bytes · content-type ${echoDirect.headers['content-type']}`,
);

// --- 5. markup in a server-supplied value stays text ------------------------------
paramInput().value = '<img src=x onerror=alert(1)>';
submit();
await waitFor('markup echo', () => text('response-body').includes('onerror'));
// `encodeURIComponent` escapes `<`, `>`, `=` and space; the page escapes `/ ? #` on top,
// while `(` and `)` stay because they are legal path characters. The `prefix` row from
// the step above still applies, which is the point: nothing is reset behind the user.
check('markup url', requests[requests.length - 1].url, '/v1/services/echo/%3Cimg%20src%3Dx%20onerror%3Dalert(1)%3E?prefix=pre-');
check('markup body', text('response-body'), '{\n  "value": "pre-<img src=x onerror=alert(1)>"\n}');
check('the markup stays a text node', element('response-body').children.length, 0);
check('no element was injected into the response panel', doc.querySelectorAll('#response-panel img, #response-panel script').length, 0);

// --- 6. invalid input is reported, and nothing is sent ----------------------------
const beforeEmptyParam = requests.length;
paramInput().value = '   ';
submit();
await waitFor('validation error', () => !element('request-error').hidden);
// A whitespace-only parameter trims to empty, which leaves the placeholder in the path;
// both facts are named instead of sending `/v1/services/echo/{value}`.
check(
    'empty parameter message',
    text('request-error'),
    'The "value" path parameter is empty. The path template contains an unsupported placeholder: /v1/services/echo/{value}.',
);
check('invalid input sent no request', requests.length, beforeEmptyParam);
check('the previous response is kept while input is invalid', text('response-body'), '{\n  "value": "pre-<img src=x onerror=alert(1)>"\n}');

// --- 7. search filters the rail without disturbing the composer -------------------
// `health` is the one term that still matches a single row now that the rail lists the
// two REST sample trees as well (its summaries say `pet`/`location`, never `health`).
element('endpoint-search').value = 'health';
element('endpoint-search').dispatchEvent(new window.Event('input', { bubbles: true }));
await waitFor('filtered rail', () => doc.querySelectorAll('#endpoint-list button').length === 1);
check('filtered count', text('endpoint-count'), '1 match of 18');
check('a filter that matches leaves the state line empty', text('endpoint-state'), '');
check('the composer keeps its selection', text('composer-target'), 'GET /v1/services/echo/{value}');
element('endpoint-search').value = 'nope';
element('endpoint-search').dispatchEvent(new window.Event('input', { bubbles: true }));
await waitFor('empty rail', () => doc.querySelectorAll('#endpoint-list button').length === 0);
check('no-match state', text('endpoint-state'), 'No endpoint matches "nope".');
element('endpoint-search').value = '';
element('endpoint-search').dispatchEvent(new window.Event('input', { bubbles: true }));
await waitFor('full rail', () => doc.querySelectorAll('#endpoint-list button').length === 18);

// --- 8. POST: the editor, the automatic content-type, the real round trip --------
const postRow = rowFor('Use POST /v1/services/echo - Echoes the posted `value` unchanged');
postRow.click();
check('a body method shows the editor', element('body-group').hidden, false);
check('the starter body for the sample endpoint', value('request-body'), '{\n  "value": "hello from the playground"\n}');
check('the body hint names the JSON rule', text('body-hint'), 'Sent as the request body, with any surrounding whitespace trimmed. It must be valid JSON - this API speaks JSON only.');
// The hint promises a trim, so the trim is pinned: a pasted body usually arrives padded
// with newlines, and what goes on the wire is the JSON without them.
element('request-body').value = '\n   {\n  "value": "round-trip"\n}   \n';
submit();
await waitFor('post response', () => text('response-body').includes('round-trip'));
check('post url', requests[requests.length - 1].url, '/v1/services/echo');
check('post method', requests[requests.length - 1].method, 'POST');
check('the content-type is applied automatically', requests[requests.length - 1].headers['content-type'], 'application/json');
check('the body is sent with the padding trimmed off', requests[requests.length - 1].body, '{\n  "value": "round-trip"\n}');
check('post response body', text('response-body'), '{\n  "value": "round-trip"\n}');

// --- 9. malformed JSON is caught before the socket is used -----------------------
const beforeBadBody = requests.length;
element('request-body').value = '{';
submit();
await waitFor('json error', () => !element('request-error').hidden);
// The parenthesised half is the engine's own SyntaxError text (it varies per engine), so
// the playground's wording is pinned exactly and only the engine part is bounded.
check('malformed json prefix', text('request-error').startsWith('The request body is not valid JSON (SyntaxError '), true);
check('malformed json suffix', text('request-error').endsWith(').'), true);
check('malformed json stays one line', /\n/.test(text('request-error')), false);
check('malformed json sent no request', requests.length, beforeBadBody);

// --- 10. an illegal header is refused loudly, never dropped silently ------------
element('request-body').value = '{"value":"x"}';
const forbiddenBefore = requests.length;
firstRowKey('#header-rows').value = 'host';
firstRowValue('#header-rows').value = 'evil.example';
submit();
await waitFor('forbidden header error', () => text('request-error').includes('cannot be edited here'));
check('forbidden header message', text('request-error'), '"host" is set by the browser and cannot be edited here.');
check('a refused header sends no request', requests.length, forbiddenBefore);
firstRowKey('#header-rows').value = 'bad header';
submit();
await waitFor('illegal name error', () => text('request-error').includes('is not a valid header name'));
check('illegal header name message', text('request-error'), '"bad header" is not a valid header name.');
element('add-header').click();
const headerRows = doc.querySelectorAll('#header-rows .kv-row');
headerRows[headerRows.length - 1].querySelector('.kv-key').value = 'x-dup';
headerRows[headerRows.length - 1].querySelector('.kv-value').value = '2';
firstRowKey('#header-rows').value = 'x-dup';
firstRowValue('#header-rows').value = '1';
submit();
await waitFor('duplicate header error', () => text('request-error').includes('more than once'));
check('duplicate header message', text('request-error'), '"x-dup" was given more than once.');
// A control character in a value is the header-injection case (a raw CR/LF cannot be
// typed: the `input` value sanitiser replaces it), and it is refused before `Headers`
// is ever asked.
firstRowKey('#header-rows').value = 'x-dup';
firstRowValue('#header-rows').value = 'line1\u0001x-evil';
submit();
await waitFor('control character error', () => text('request-error').includes('control character'));
check('control character message', text('request-error'), 'The value of "x-dup" contains a control character.');
check('no header problem ever reached the socket', requests.length, forbiddenBefore);
// Clear the header rows again so the cancel/timeout steps below send a clean request.
// Removing the last row re-seeds exactly one blank row (the affordance never disappears).
[...doc.querySelectorAll('#header-rows .remove-row')].forEach((button) => button.click());
check('removing the last row re-seeds one blank row', doc.querySelectorAll('#header-rows .kv-row').length, 1);

// A header name that collides with an `Object.prototype` member is an ordinary header.
// With object-literal lookups `FORBIDDEN_HEADERS["constructor"]` and `seen["constructor"]`
// both answered truthy, so this name was misdiagnosed as browser-owned on its FIRST use;
// the null-prototype tables have to let it through and only call it a duplicate when the
// composer really holds the name twice.
const beforePrototypeHeader = requests.length;
firstRowKey('#header-rows').value = 'constructor';
firstRowValue('#header-rows').value = 'x';
submit();
await waitFor('the constructor header to go out', () => requests.length === beforePrototypeHeader + 1);
check('a prototype-named header is not refused', text('request-error'), '');
check('the prototype-named header is sent', requests[requests.length - 1].headers['constructor'], 'x');
// The answer has to arrive before the next submit: while a request is in flight Send is
// locked (see the busy note), which is exactly what the step below relies on.
await waitFor('the answer to the constructor request', () => text('response-body') === '{\n  "value": "x"\n}');
check('and it was answered', text('response-body'), '{\n  "value": "x"\n}');
check('Send is free again after the answer', element('send').disabled, false);
element('add-header').click();
const prototypeRows = doc.querySelectorAll('#header-rows .kv-row');
prototypeRows[prototypeRows.length - 1].querySelector('.kv-key').value = 'constructor';
prototypeRows[prototypeRows.length - 1].querySelector('.kv-value').value = 'y';
submit();
await waitFor('the duplicate constructor error', () => text('request-error').includes('more than once'));
check('a repeated prototype-named header is a real duplicate', text('request-error'), '"constructor" was given more than once.');
check('the duplicate was not sent either', requests.length, beforePrototypeHeader + 1);

// --- 11. a POST without a body reaches the server and reports its answer ---------
postRow.click();
element('request-body').value = '';
submit();
await waitFor('415 answer', () => text('status-pill') === LABEL_415);
check('no body means no content-type', requests[requests.length - 1].headers['content-type'], undefined);
check('415 body', text('response-body'), REJECT_UNSUPPORTED_MEDIA);
check('415 pill tone', tone('status-pill'), 'error');
check('415 state line', text('response-state'), 'The service answered 415.');

// --- 12. a 4xx is rendered as the service's own answer, not swallowed -----------
postRow.click();
element('request-body').value = '{"value":{"a":1}}';
submit();
await waitFor('422 body', () => text('response-body').includes('Failed to deserialize'));
check('422 pill', text('status-pill'), LABEL_422);
check('422 pill tone', tone('status-pill'), 'error');
check('422 state line', text('response-state'), 'The service answered 422.');
check(
    '422 body is the rejection text',
    text('response-body'),
    'Failed to deserialize the JSON body into the target type: value: invalid type: map, expected a string at line 1 column 9',
);

// --- 13. transport failure: the request produced no response at all --------------
const realFetch = window.fetch;
const rejectTransport = (call) => {
    requests.push(call);
    return Promise.reject(new TypeError('Failed to fetch'));
};
window.fetch = (input) => rejectTransport({ method: 'POST', url: typeof input === 'string' ? input : input.url });
postRow.click();
element('request-body').value = '{"value":"x"}';
submit();
await waitFor('failure state', () => text('response-state').startsWith('The request never produced a response:'));
check('failure pill', text('status-pill'), 'failed');
check('failure pill tone', tone('status-pill'), 'error');
check(
    'failure state',
    text('response-state'),
    'The request never produced a response: TypeError Failed to fetch. The service may be stopped, the URL may redirect outside this origin, or the request was blocked.',
);
check('a failed request clears the body', text('response-body'), '');
check('a failed request re-enables Send', element('send').disabled, false);
window.fetch = realFetch;

// --- 14. cancel: an explicit state, and the composer works again after ----------
const abortingTransport = (_call, init) =>
    new Promise((_resolve, reject) => {
        init.signal.addEventListener('abort', () =>
            reject(new window.DOMException('The operation was aborted.', 'AbortError')),
        );
    });
window.fetch = (input, init) => abortingTransport(null, init);
postRow.click();
element('request-body').value = '{"value":"x"}';
submit();
await waitFor('busy state', () => element('send').disabled === true);
check('Cancel is offered while a request is in flight', element('cancel').hidden, false);
check('busy note', text('actions-note'), 'Request in flight. Esc or Cancel stops it.');
check('Send is aria-busy', element('send').getAttribute('aria-busy'), 'true');
element('cancel').click();
await waitFor('cancelled state', () => text('status-pill') === 'cancelled');
check('cancelled state', text('response-state'), 'Request cancelled before the service answered.');
check('Send works again after a cancel', element('send').disabled, false);
check('idle note', text('actions-note'), 'Ctrl+Enter sends, Esc cancels.');
window.fetch = realFetch;

// --- 15. timeout: the 30s watchdog aborts with its own state --------------------
// The watchdog is installed through `window.setTimeout`, so the real 30s wait is
// replaced by a captured callback: the arming delay is asserted, then fired by hand.
const realSetTimeout = window.setTimeout;
const realClearTimeout = window.clearTimeout;
let watchdog = null;
Object.defineProperty(window, 'setTimeout', {
    configurable: true,
    value: (callback, ms) => {
        watchdog = { callback, ms };
        return 0;
    },
});
Object.defineProperty(window, 'clearTimeout', {
    configurable: true,
    value: () => {
        watchdog = null;
    },
});
window.fetch = (input, init) => abortingTransport(null, init);
postRow.click();
element('request-body').value = '{"value":"x"}';
submit();
await waitFor('the watchdog to arm', () => watchdog !== null);
check('the watchdog delay is the documented 30s', watchdog.ms, 30000);
watchdog.callback();
await waitFor('timeout state', () => text('status-pill') === 'timeout');
check('timeout state', text('response-state'), 'No response within 30s; the request was aborted.');
check('timeout pill tone', tone('status-pill'), 'error');
window.fetch = realFetch;
Object.defineProperty(window, 'setTimeout', { configurable: true, writable: true, value: realSetTimeout });
Object.defineProperty(window, 'clearTimeout', { configurable: true, writable: true, value: realClearTimeout });

// --- 16. Clear returns the response panel to its untouched text -----------------
element('clear').click();
check('cleared state', text('response-state'), 'No request has been sent yet.');
check('cleared body', text('response-body'), '');
check('cleared pill', text('status-pill'), '');
check('cleared url fact', text('fact-url'), '');
check('cleared timing', text('response-timing'), '');

// --- 17. a listing that fails leaves a usable, honest page, and a way back -------
const brokenDom = new JSDOM(assets.served['/'], {
    url: BASE || 'http://127.0.0.1:5001/',
    contentType: 'text/html',
    runScripts: 'outside-only',
    pretendToBeVisual: true,
    virtualConsole,
});
instrument(brokenDom.window, () => Promise.reject(new TypeError('Failed to fetch')));
brokenDom.window.eval(assets.served['/playground.js']);
await new Promise((resolve) => setTimeout(resolve, 150));
const brokenDoc = brokenDom.window.document;
const brokenText = (id) => brokenDoc.getElementById(id).textContent;
check('listing failure state', brokenText('endpoint-state'), 'Could not load the endpoint table: TypeError Failed to fetch');
check('listing failure empties the rail', brokenDoc.querySelectorAll('#endpoint-list button').length, 0);
check('listing failure collapses the composer', brokenText('composer-state'), 'Select an endpoint to begin.');
check('listing failure disables Send', brokenDoc.getElementById('send').disabled, true);
check('listing failure offers a retry', brokenDoc.getElementById('reload-endpoints').hidden, false);

// The precise reason has to survive a re-render: `renderList` runs on every search
// keystroke, and the generic "nothing was reported" line used to win from there on.
brokenDoc.getElementById('endpoint-search').value = 'nope';
brokenDoc.getElementById('endpoint-search').dispatchEvent(new brokenDom.window.Event('input', { bubbles: true }));
check('a keystroke keeps the precise load error', brokenText('endpoint-state'), 'Could not load the endpoint table: TypeError Failed to fetch');
check('the retry stays offered next to it', brokenDoc.getElementById('reload-endpoints').hidden, false);
brokenDoc.getElementById('endpoint-search').value = '';
brokenDoc.getElementById('endpoint-search').dispatchEvent(new brokenDom.window.Event('input', { bubbles: true }));

// The retry is the same one request, in place: no page reload, and the composer state
// (the collapsed one here) is re-derived from the listing that now answers.
const retried = instrument(brokenDom.window);
brokenDoc.getElementById('reload-endpoints').click();
await waitFor('the retried rail', () => brokenDoc.querySelectorAll('#endpoint-list button').length === 18);
check('the retry issued only the listing request', retried.length, 1);
check('the retry requested the listing', retried[0].url, '/v1/services/routes');
check('the retry cleared the error line', brokenText('endpoint-state'), '');
check('the retry hid itself', brokenDoc.getElementById('reload-endpoints').hidden, true);
check('the retry reported the count', brokenText('endpoint-count'), '18 endpoints');
check('the retry enabled the composer', brokenDoc.getElementById('send').disabled, false);
check('the retry pre-loaded the first entry', brokenText('composer-target'), 'ANY /v1/harness');
brokenDom.window.close();

// --- 18. the page's own safety rules -------------------------------------------
check('no script errors while driving the page', consoleErrors.join('\n'), '');
check('the served document has no inline event handler', /on(click|error|load|mouse\w+|submit)=/.test(assets.served['/']), false);
check('the served document has no inline style attribute', /\sstyle=/.test(assets.served['/']), false);
check('the script never writes markup', /\binnerHTML\b|\bouterHTML\b|document\.write|insertAdjacentHTML/.test(assets.served['/playground.js']), false);
check('the script fetches no second origin', /(^|[^"'])https?:\/\//.test(assets.served['/playground.js']), false);
check('the script never uses eval', /\beval\s*\(/.test(assets.served['/playground.js']), false);

window.close();

console.log(`\n${results.length - failures}/${results.length} checks passed (${BASE ? `live ${BASE}` : 'hermetic fixtures'})`);
if (failures) {
    console.log(`${failures} FAILED`);
    process.exitCode = 1;
}
