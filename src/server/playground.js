/*
 * Playground behaviour for `typecrate-services` (T18).
 *
 * Served as an embedded `include_str!` asset at `/playground.js` (see
 * `src/server/mod.rs`). Rules this file is written against, and why:
 *
 *   * No framework, no build step, no external module: the service serves this
 *     document itself, so the tool works with the network cut down to the one
 *     origin it is loaded from (`script-src 'self'` in the CSP).
 *   * Nothing is ever injected as markup. Every string that came from the server
 *     (endpoint summaries/paths, response bodies, error text) goes through
 *     `textContent` or `createTextNode`, so a route whose summary contains
 *     `<img onerror=...>` displays those characters instead of executing them.
 *   * Every request is same-origin by construction: the URL handed to `fetch` is
 *     a relative absolute-path string built from a validated template, and
 *     `mode: "same-origin"` + `redirect: "error"` mean a cross-origin answer or
 *     any redirect fails loudly instead of leaving the page's origin.
 *   * Nothing destructive is sent automatically. The only load-time request is
 *     the `GET` endpoint listing; a body is only sent when the user presses
 *     Send, and the HTTP method always comes from the listing (never typed).
 */
(function () {
    "use strict";

    // ------------------------------------------------------------------ config

    /** The builtin metadata route (must match `server::ROUTE_METADATA_PATH`). */
    var ROUTE_METADATA_URL = "/v1/services/routes";

    /**
     * Client-side watchdog. The server has no request timeout of its own, so a
     * hung handler would otherwise leave the Send button disabled forever; 30s is
     * long enough for a slow local handler and short enough to notice.
     */
    var REQUEST_TIMEOUT_MS = 30000;

    /** Display cap for a response body: a huge payload is shown truncated. */
    var MAX_RENDERED_BODY_CHARS = 200000;

    /**
     * A lookup table that answers only for its OWN keys.
     *
     * Method names and header names are untrusted text, and a plain `{}` literal answers
     * truthy for every inherited `Object.prototype` member: `FORBIDDEN_HEADERS["constructor"]`
     * would call a legal header "set by the browser", and a first row named `toString`
     * would be reported as a duplicate before it was ever sent. `Object.create(null)`
     * turns those names into ordinary keys, which is what makes the diagnostics below
     * report only what is actually true.
     */
    function table(pairs) {
        var built = Object.create(null);
        pairs.forEach(function (pair) {
            built[pair[0]] = pair[1];
        });
        return built;
    }

    /** Only these methods get a body editor (GET/HEAD/DELETE/OPTIONS never do here). */
    var METHODS_WITH_BODY = table([["POST", true], ["PUT", true], ["PATCH", true]]);

    /**
     * Starter bodies for this package's own sample endpoints, keyed by
     * "<METHOD> <path template>". An unlisted body-accepting method falls back to
     * `{}`, so a new endpoint needs no code change - only an entry when it wants a
     * nicer default.
     */
    var DEFAULT_BODIES = {
        "POST /v1/services/echo": '{\n  "value": "hello from the playground"\n}'
    };

    /** RFC 7230 token, the only legal shape for a header name. */
    var HEADER_NAME_RE = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

    /** Header values may not contain control characters (CR/LF injection guard). */
    var HEADER_VALUE_RE = /^[\x20-\x7E\x80-\xFF]*$/;

    /** axum 0.8 / matchit path template parameter, e.g. `{value}`. */
    var PARAM_RE = /\{([A-Za-z_][A-Za-z0-9_]*)\}/g;

    /**
     * Names the fetch spec forbids a page from setting. The browser would drop
     * them silently, so the playground refuses the row with a message instead of
     * letting the user believe the header went out.
     *
     * Built through [`table`] on purpose: the lookup key is the lower-cased header name
     * a person typed, and `constructor`, `toString` and `__proto__` are all legal header
     * names that an object literal would answer truthy for, misdiagnosing them as
     * browser-owned.
     */
    var FORBIDDEN_HEADERS = table([
        ["content-length", true],
        ["cookie", true],
        ["cookie2", true],
        ["host", true],
        ["origin", true],
        ["referer", true],
        ["accept-encoding", true],
        ["connection", true],
        ["keep-alive", true],
        ["upgrade", true]
    ]);

    // --------------------------------------------------------------------- dom

    function id(name) {
        return document.getElementById(name);
    }

    var ui = {
        endpointCount: id("endpoint-count"),
        endpointSearch: id("endpoint-search"),
        endpointState: id("endpoint-state"),
        endpointList: id("endpoint-list"),
        // The failed-listing affordance: re-runs `loadEndpoints` in place.
        reloadEndpoints: id("reload-endpoints"),
        composerState: id("composer-state"),
        composerTarget: id("composer-target"),
        form: id("request-form"),
        method: id("request-method"),
        path: id("request-path"),
        summary: id("request-summary"),
        paramGroup: id("path-param-group"),
        paramRows: id("path-param-rows"),
        queryRows: id("query-rows"),
        addQuery: id("add-query"),
        headerRows: id("header-rows"),
        addHeader: id("add-header"),
        bodyGroup: id("body-group"),
        body: id("request-body"),
        bodyHint: id("body-hint"),
        send: id("send"),
        cancel: id("cancel"),
        clear: id("clear"),
        actionsNote: id("actions-note"),
        requestError: id("request-error"),
        responseState: id("response-state"),
        responseFacts: id("response-facts"),
        statusPill: id("status-pill"),
        factUrl: id("fact-url"),
        responseTiming: id("response-timing"),
        headersHead: id("headers-head"),
        responseHeaders: id("response-headers"),
        bodyHead: id("body-head"),
        bodySize: id("body-size"),
        responseBody: id("response-body")
    };

    // ------------------------------------------------------------------- state

    var state = {
        /** Validated entries from `GET /v1/services/routes` (registration order). */
        entries: [],
        /** Entries the listing returned but this page refused to use. */
        rejected: [],
        /**
         * The reason the last listing load FAILED, as display text ("" when the last
         * load succeeded or none has failed yet).
         *
         * It lives in state rather than being written once into the status line because
         * `renderList` re-runs on every search keystroke: an empty rail would otherwise
         * replace the precise transport error with the generic "nothing was reported"
         * line on the first keypress, and that detail is the whole point of the message.
         */
        loadError: "",
        /** The selected entry; `null` until the user picks one. */
        selected: null,
        controller: null,
        timer: null,
        busy: false,
        timedOut: false,
        /** Monotonic id so generated rows keep stable `for`-free aria labels. */
        rowSeq: 0
    };

    // ------------------------------------------------------------- small utils

    /** `performance.now()` with a Date fallback, for elapsed-time measurement. */
    function nowMs() {
        return window.performance && typeof window.performance.now === "function"
            ? window.performance.now()
            : Date.now();
    }

    /**
     * Assigns text without ever parsing it. `textContent` is the whole XSS story:
     * nothing below reaches the parser.
     */
    function text(node, value) {
        node.textContent = value === null || value === undefined ? "" : String(value);
    }

    /** Shows/hides a node with the `hidden` attribute (no inline styles, no CSS). */
    function show(node, visible) {
        node.hidden = !visible;
    }

    /** One status line for a panel; `tone` is a data attribute, never a style. */
    function setState(node, message, tone) {
        text(node, message);
        if (tone) {
            node.setAttribute("data-tone", tone);
        } else {
            node.removeAttribute("data-tone");
        }
    }

    /** Pluralise a count label ("2 endpoints" / "1 endpoint"). */
    function plural(count, one, many) {
        return count + " " + (count === 1 ? one : many);
    }

    /** Error -> single-line text (`AbortError` etc. have no useful stack here). */
    function describeError(error) {
        if (!error) {
            return "unknown error";
        }
        var name = error.name ? String(error.name) : "Error";
        var message = error.message ? " " + String(error.message) : "";
        return name + message;
    }

    /**
     * Drops every child of a node by removing nodes, never by replacing the
     * subtree with a parsed string (which is how untrusted text becomes an element).
     */
    function clearChildren(node) {
        while (node.firstChild) {
            node.removeChild(node.firstChild);
        }
    }

    /**
     * True only for a URL that can be fetched without leaving this origin.
     *
     * Rejected: `//host/x` (protocol-relative, i.e. another origin), anything with
     * a scheme, `\` (some user agents normalise it to `/`), control characters and
     * the quote/angle characters that a redirect target could smuggle in, plus `?`
     * and `#` in a path template (they would move or truncate the rest of the
     * string, so the query could no longer be built separately).
     */
    function isSameOriginApiPath(value) {
        if (typeof value !== "string") {
            return false;
        }
        if (value.charAt(0) !== "/" || value.charAt(1) === "/") {
            return false;
        }
        if (value.indexOf("\\") !== -1 || /[\x00-\x1F\x7F<>"'`?#]/.test(value)) {
            return false;
        }
        // A template path may still contain a colon inside a segment (`/v1/a:b`),
        // which is harmless; only a *leading* scheme could change the origin.
        return !/^[A-Za-z][A-Za-z0-9+.\-]*:/.test(value.slice(1));
    }

    /** The same-origin invariant re-checked on the finished URL (defence in depth). */
    function isSafeRequestTarget(value) {
        return (
            typeof value === "string" &&
            value.charAt(0) === "/" &&
            value.charAt(1) !== "/" &&
            value.indexOf("\\") === -1 &&
            !/[\x00-\x1F\x7F<>"]/.test(value)
        );
    }

    /**
     * Encodes one path segment. `encodeURIComponent` leaves `/ ? #` alone, and any
     * of those would change the *routing*, not just the value, so they are escaped
     * as well (`%2F` is decoded back to `/` by the extractor, which is the
     * documented behaviour of the sample echo endpoint).
     */
    function encodePathSegment(value) {
        return encodeURIComponent(value).replace(/[/?#]/g, function (character) {
            return "%" + character.charCodeAt(0).toString(16).toUpperCase();
        });
    }

    // --------------------------------------------------- metadata (endpoint list)

    /**
     * Validates the metadata payload into the shape the page can use.
     *
     * A single malformed entry must not blank the whole list, so bad entries are
     * collected and reported separately. `method` is upper-cased only for
     * comparison/display rules; the string sent to `fetch` is the server's own.
     */
    function normaliseEntries(value) {
        var entries = [];
        var rejected = [];
        if (!Array.isArray(value)) {
            return { entries: entries, rejected: ["Expected a JSON array, got " + (value === null ? "null" : typeof value) + "."] };
        }
        value.forEach(function (raw, index) {
            if (!raw || typeof raw !== "object") {
                rejected.push("Entry " + index + " is not an object.");
                return;
            }
            var method = typeof raw.method === "string" ? raw.method : "";
            var path = typeof raw.path === "string" ? raw.path : "";
            var summary = typeof raw.summary === "string" ? raw.summary : "";
            if (!/^[A-Za-z]+$/.test(method)) {
                rejected.push("Entry " + index + ' has an unusable method "' + method + '".');
                return;
            }
            if (!isSameOriginApiPath(path)) {
                rejected.push('Entry ' + index + ' has a path this page will not request: "' + path + '".');
                return;
            }
            entries.push({ method: method, path: path, summary: summary });
        });
        return { entries: entries, rejected: rejected };
    }

    /** Loads the endpoint table. This is the only request made without user action. */
    function loadEndpoints() {
        setState(ui.endpointState, "Loading the endpoint table…", "busy");
        // A retry is in flight, so the stale reason is replaced by the busy line. If
        // this attempt fails, `catch` writes a fresh, equally precise `loadError`; a
        // success leaves it empty, which is what hides the retry affordance again.
        state.loadError = "";
        show(ui.reloadEndpoints, false);
        text(ui.endpointCount, "");
        fetch(ROUTE_METADATA_URL, {
            method: "GET",
            headers: { accept: "application/json" },
            credentials: "same-origin",
            // Same-origin mode + `redirect: "error"` together mean the listing can
            // never be answered by another origin, even via a 3xx.
            mode: "same-origin",
            redirect: "error",
            cache: "no-store"
        })
            .then(function (response) {
                if (!response.ok) {
                    return response.text().then(function (body) {
                        throw new Error(
                            "GET " + ROUTE_METADATA_URL + " returned " + response.status +
                            (body ? " (" + body.slice(0, 160).replace(/\s+/g, " ").trim() + ")" : "")
                        );
                    });
                }
                return response.json();
            })
            .then(function (value) {
                var parsed = normaliseEntries(value);
                state.entries = parsed.entries;
                state.rejected = parsed.rejected;
                // Selection FIRST, then the render: `renderList` marks the row that
                // matches `state.selected` with `aria-current`, and the listing
                // auto-selects its first entry, so rendering before selecting would
                // leave the composer showing an entry the rail does not mark.
                selectEntry(state.entries.length ? state.entries[0] : null);
                renderList();
            })
            .catch(function (error) {
                state.entries = [];
                state.rejected = [];
                // The reason goes into state, not just into the visible line: `renderList`
                // re-runs on every search keystroke, and this is the only thing that keeps
                // the precise transport error from being replaced by the generic
                // "nothing was reported" line the moment the user touches the filter.
                state.loadError = "Could not load the endpoint table: " + describeError(error);
                selectEntry(null);
                renderList();
            });
    }

    /** Case-insensitive substring match over the three metadata fields. */
    function matches(entry, term) {
        if (!term) {
            return true;
        }
        var haystack = (entry.method + " " + entry.path + " " + entry.summary).toLowerCase();
        return haystack.indexOf(term) !== -1;
    }

    /**
     * Rebuilds the rail from `state.entries` filtered by the search box.
     *
     * The list is recreated on every keystroke: the table is a handful of entries,
     * and rebuilding is what guarantees a stale entry can never keep a click
     * handler pointing at an entry that is no longer displayed.
     */
    function renderList() {
        var term = ui.endpointSearch.value.trim().toLowerCase();
        var visible = state.entries.filter(function (entry) {
            return matches(entry, term);
        });

        clearChildren(ui.endpointList);
        visible.forEach(function (entry) {
            var item = document.createElement("li");
            var button = document.createElement("button");
            button.type = "button";
            button.className = "endpoint";
            if (state.selected && state.selected.path === entry.path && state.selected.method === entry.method) {
                button.setAttribute("aria-current", "true");
            }

            var badge = document.createElement("span");
            badge.className = "badge";
            // The method becomes a *class-selected* colour via a data attribute,
            // so no style value is ever derived from server text.
            badge.setAttribute("data-method", entry.method.toLowerCase());
            text(badge, entry.method);

            var path = document.createElement("span");
            path.className = "endpoint-path";
            text(path, entry.path);

            var summary = document.createElement("span");
            summary.className = "endpoint-summary";
            text(summary, entry.summary || "No summary.");

            button.appendChild(badge);
            button.appendChild(path);
            button.appendChild(summary);
            button.setAttribute(
                "aria-label",
                "Use " + entry.method + " " + entry.path + (entry.summary ? " - " + entry.summary : "")
            );
            // Closure over `entry` (not an index): filtering changes indexes.
            button.addEventListener("click", function () {
                selectEntry(entry);
                renderList();
            });
            item.appendChild(button);
            ui.endpointList.appendChild(item);
        });

        var total = state.entries.length;
        text(
            ui.endpointCount,
            total === 0 ? "" : term ? plural(visible.length, "match", "matches") + " of " + total : plural(total, "endpoint", "endpoints")
        );

        // Loading is handled by loadEndpoints; these are the settled states.
        if (total === 0) {
            // A load that FAILED keeps its own, precise reason (see `state.loadError`);
            // the generic line is only for a listing that answered successfully with
            // nothing usable in it, which is a different problem to debug.
            setState(
                ui.endpointState,
                state.loadError ||
                    "No usable endpoint was reported by " + ROUTE_METADATA_URL + ".",
                "error"
            );
        } else if (visible.length === 0) {
            setState(ui.endpointState, 'No endpoint matches "' + term + '".', null);
        } else if (state.rejected.length) {
            setState(ui.endpointState, plural(state.rejected.length, "entry", "entries") + " refused: " + state.rejected.join(" "), "error");
        } else {
            setState(ui.endpointState, "", null);
        }

        // The retry affordance exists exactly while a load error is on screen, next to
        // the reason it happened, and disappears on the first successful render.
        show(ui.reloadEndpoints, state.loadError !== "");
    }

    // ---------------------------------------------------------------- composer

    /**
     * Loads `entry` into the composer, or collapses it when `entry` is null
     * (nothing usable in the listing). Path parameters, the body editor and the
     * row sets are all rebuilt from the template, never assumed.
     */
    function selectEntry(entry) {
        state.selected = entry;
        clearRequestError();
        clearResponse();

        if (!entry) {
            show(ui.form, false);
            // `value`, not `textContent`: these are readonly inputs and an input's
            // displayed content is its value, never its text node. `text()` below is
            // for elements that actually hold text.
            ui.method.value = "";
            ui.path.value = "";
            text(ui.summary, "");
            text(ui.composerTarget, "");
            setState(ui.composerState, "Select an endpoint to begin.", null);
            ui.send.disabled = true;
            return;
        }

        show(ui.form, true);
        ui.send.disabled = false;
        setState(ui.composerState, "", null);
        ui.method.value = entry.method;
        ui.path.value = entry.path;
        text(ui.summary, entry.summary || "");
        text(ui.composerTarget, entry.method + " " + entry.path);

        buildParamRows(entry.path);

        var acceptsBody = !!METHODS_WITH_BODY[entry.method.toUpperCase()];
        show(ui.bodyGroup, acceptsBody);
        if (acceptsBody) {
            ui.body.value = DEFAULT_BODIES[entry.method.toUpperCase() + " " + entry.path] || "{}";
            // Honest wording: `collectBody` trims, so a padded editor buffer is sent
            // without the padding (JSON does not care, and a stray newline in a pasted
            // body would otherwise be invisible in the request).
            text(ui.bodyHint, "Sent as the request body, with any surrounding whitespace trimmed. It must be valid JSON - this API speaks JSON only.");
        } else {
            // Clearing avoids a stale body silently riding along if the user
            // switches from POST to GET.
            ui.body.value = "";
            text(ui.bodyHint, "");
        }

        // One blank row each: enough to show the affordance, nothing to guess.
        clearChildren(ui.queryRows);
        addRow(ui.queryRows, "Query parameter name", "value", "", "");
        clearChildren(ui.headerRows);
        addRow(ui.headerRows, "Header name", "header value", "", "");
    }

    /** Unique `{name}` placeholders in a path template, in the order they appear. */
    function paramNames(path) {
        var names = [];
        var matcher = new RegExp(PARAM_RE.source, "g");
        var found = matcher.exec(path);
        while (found !== null) {
            if (names.indexOf(found[1]) === -1) {
                names.push(found[1]);
            }
            found = matcher.exec(path);
        }
        return names;
    }

    /** One labelled, required input per path parameter of the selected template. */
    function buildParamRows(path) {
        var names = paramNames(path);
        clearChildren(ui.paramRows);
        show(ui.paramGroup, names.length > 0);
        names.forEach(function (name) {
            var row = document.createElement("div");
            row.className = "param-row";
            row.setAttribute("data-param", name);

            var label = document.createElement("label");
            label.className = "field-label";
            var inputId = "param-" + name + "-" + (state.rowSeq += 1);
            label.setAttribute("for", inputId);
            text(label, name);

            var input = document.createElement("input");
            input.className = "field";
            input.id = inputId;
            input.type = "text";
            input.required = true;
            input.autocomplete = "off";
            input.spellcheck = false;
            input.placeholder = "value for {" + name + "}";

            row.appendChild(label);
            row.appendChild(input);
            ui.paramRows.appendChild(row);
        });
    }

    /**
     * Appends a key/value row (query parameters and headers share the shape).
     * Labels are `aria-label`s because the fieldset legend already names the set.
     */
    function addRow(container, keyPlaceholder, valuePlaceholder, key, value) {
        state.rowSeq += 1;
        var row = document.createElement("div");
        row.className = "kv-row";

        var keyInput = document.createElement("input");
        keyInput.className = "field kv-key";
        keyInput.type = "text";
        keyInput.autocomplete = "off";
        keyInput.spellcheck = false;
        keyInput.placeholder = keyPlaceholder;
        keyInput.setAttribute("aria-label", keyPlaceholder);
        keyInput.value = key || "";

        var valueInput = document.createElement("input");
        valueInput.className = "field kv-value";
        valueInput.type = "text";
        valueInput.autocomplete = "off";
        valueInput.spellcheck = false;
        valueInput.placeholder = valuePlaceholder;
        valueInput.setAttribute("aria-label", valuePlaceholder);
        valueInput.value = value || "";

        var remove = document.createElement("button");
        remove.type = "button";
        remove.className = "remove-row";
        text(remove, "Remove");
        remove.setAttribute("aria-label", "Remove this " + keyPlaceholder.toLowerCase());

        row.appendChild(keyInput);
        row.appendChild(valueInput);
        row.appendChild(remove);
        container.appendChild(row);
        return row;
    }

    /** Reads the two inputs of every row; fully blank rows are dropped. */
    function readRows(container) {
        var pairs = [];
        var rows = container.querySelectorAll(".kv-row");
        Array.prototype.forEach.call(rows, function (row) {
            var key = row.querySelector(".kv-key").value.trim();
            var value = row.querySelector(".kv-value").value;
            if (key !== "") {
                pairs.push({ key: key, value: value, row: row });
            }
        });
        return pairs;
    }

    // ------------------------------------------------------------- url building

    /**
     * Turns the selected template into the URL actually fetched.
     *
     * Every problem is pushed into `problems` and reported in the composer, never
     * guessed at: a missing path parameter, a value that would change the routing,
     * or a template this page refuses to request.
     */
    function buildUrl(entry, problems) {
        var values = {};
        var rows = ui.paramRows.querySelectorAll(".param-row");
        Array.prototype.forEach.call(rows, function (row) {
            values[row.getAttribute("data-param")] = row.querySelector("input").value.trim();
        });

        var matcher = new RegExp(PARAM_RE.source, "g");
        var urlPath = entry.path.replace(matcher, function (whole, name) {
            var value = values[name];
            if (value === undefined || value === "") {
                problems.push('The "' + name + '" path parameter is empty.');
                return whole;
            }
            return encodePathSegment(value);
        });

        // Anything still brace-shaped is a placeholder this page does not know how
        // to fill; sending it literally would ask for a different route.
        if (urlPath.indexOf("{") !== -1) {
            problems.push("The path template contains an unsupported placeholder: " + urlPath + ".");
        }

        var params = new URLSearchParams();
        readRows(ui.queryRows).forEach(function (pair) {
            // `append` keeps repeated keys and does the encoding, so a query value
            // can never introduce `&`, `#` or a new origin.
            params.append(pair.key, pair.value);
        });
        var query = params.toString();
        var url = query ? urlPath + "?" + query : urlPath;

        if (!isSameOriginApiPath(urlPath)) {
            problems.push("The path " + entry.path + " is not a same-origin absolute path.");
        }
        if (problems.length === 0 && !isSafeRequestTarget(url)) {
            problems.push("The request URL built from this endpoint is not safe to send: " + url + ".");
        }
        return url;
    }

    // ------------------------------------------------------------ body/headers

    /** The body to send, or null. JSON is validated *before* the socket is used. */
    function collectBody(entry, problems) {
        if (!METHODS_WITH_BODY[entry.method.toUpperCase()]) {
            return null;
        }
        var raw = ui.body.value.trim();
        if (raw === "") {
            return null;
        }
        try {
            JSON.parse(raw);
        } catch (error) {
            problems.push("The request body is not valid JSON (" + describeError(error) + ").");
            return raw;
        }
        return raw;
    }

    /**
     * Builds the request headers.
     *
     * `content-type` is applied automatically for a JSON body unless the user set
     * their own (then theirs wins - that is how a non-JSON experiment is possible).
     * An invalid name, a control character in a value, or a forbidden name is a
     * reported problem rather than a silently dropped header.
     */
    function collectHeaders(entry, problems, body) {
        var headers = new Headers();
        // Same reason as `FORBIDDEN_HEADERS`: the key is a lower-cased header name from
        // the composer, and `{}` would report `constructor`/`__proto__` as already seen.
        var seen = Object.create(null);
        readRows(ui.headerRows).forEach(function (pair) {
            var name = pair.key;
            var lower = name.toLowerCase();
            if (!HEADER_NAME_RE.test(name)) {
                problems.push('"' + name + '" is not a valid header name.');
                pair.row.classList.add("is-invalid");
                return;
            }
            if (!HEADER_VALUE_RE.test(pair.value)) {
                problems.push('The value of "' + name + '" contains a control character.');
                pair.row.classList.add("is-invalid");
                return;
            }
            if (FORBIDDEN_HEADERS[lower]) {
                problems.push('"' + name + '" is set by the browser and cannot be edited here.');
                pair.row.classList.add("is-invalid");
                return;
            }
            if (seen[lower]) {
                problems.push('"' + name + '" was given more than once.');
                return;
            }
            seen[lower] = true;
            try {
                headers.append(name, pair.value);
            } catch (error) {
                // Headers itself is the last word on legality; surface its verdict.
                problems.push('The header "' + name + '" was refused: ' + describeError(error));
            }
        });

        if (body !== null && body !== undefined && !seen["content-type"]) {
            headers.append("content-type", "application/json");
        }
        return headers;
    }

    // --------------------------------------------------------------- validation

    function showRequestError(message) {
        text(ui.requestError, message);
        show(ui.requestError, true);
    }

    function clearRequestError() {
        show(ui.requestError, false);
        text(ui.requestError, "");
        var invalid = ui.form.querySelectorAll(".is-invalid");
        Array.prototype.forEach.call(invalid, function (node) {
            node.classList.remove("is-invalid");
        });
    }

    // ------------------------------------------------------------------- send

    /** Disables Send, offers Cancel, marks the form busy while a request is live. */
    function setBusy(busy) {
        state.busy = busy;
        ui.send.disabled = busy || !state.selected;
        ui.send.setAttribute("aria-busy", busy ? "true" : "false");
        show(ui.cancel, busy);
        // The note describes what is actually possible at this moment. While a request
        // is in flight `send()` returns early (no queue), so the only live action is the
        // abort - promising "Ctrl+Enter sends again" here would be a lie.
        text(ui.actionsNote, busy ? "Request in flight. Esc or Cancel stops it." : "Ctrl+Enter sends, Esc cancels.");
    }

    function stopTimer() {
        if (state.timer !== null) {
            window.clearTimeout(state.timer);
            state.timer = null;
        }
    }

    function abortInFlight() {
        if (state.controller) {
            try {
                state.controller.abort();
            } catch (error) {
                // An already-settled controller is fine; nothing to report.
            }
        }
    }

    /**
     * The single place a `fetch` is issued for a user request.
     *
     * `credentials: "same-origin"` sends cookies only to this origin (the default
     * in modern browsers, spelled out because the tool is now LAN-reachable), and
     * `redirect: "error"` + `mode: "same-origin"` are what make "cannot leave this
     * origin" true for the *response* and not just the request URL.
     */
    function send() {
        var entry = state.selected;
        if (!entry) {
            showRequestError("Select an endpoint first.");
            return;
        }
        if (state.busy) {
            return;
        }

        clearRequestError();
        var problems = [];
        var body = collectBody(entry, problems);
        var url = buildUrl(entry, problems);
        var headers = collectHeaders(entry, problems, body);
        if (problems.length) {
            // Nothing was sent: fix the composer instead of showing a server error.
            showRequestError(problems.join(" "));
            return;
        }

        var started = nowMs();
        setBusy(true);
        state.timedOut = false;
        state.controller = typeof AbortController === "function" ? new AbortController() : null;
        stopTimer();
        if (state.controller) {
            state.timer = window.setTimeout(function () {
                state.timedOut = true;
                abortInFlight();
            }, REQUEST_TIMEOUT_MS);
        }

        var init = {
            method: entry.method,
            headers: headers,
            credentials: "same-origin",
            mode: "same-origin",
            redirect: "error",
            cache: "no-store",
            referrerPolicy: "no-referrer"
        };
        if (state.controller) {
            init.signal = state.controller.signal;
        }
        if (body !== null) {
            init.body = body;
        }

        setState(ui.responseState, entry.method + " " + url + " - waiting for a response…", "busy");
        text(ui.responseTiming, "");

        fetch(url, init)
            .then(readResponse)
            .then(function (result) {
                renderResponse(entry, url, result, started);
            })
            .catch(function (error) {
                renderFailure(entry, url, error, started);
            })
            .then(function () {
                stopTimer();
                state.controller = null;
                setBusy(false);
            });
    }

    /**
     * Reads the body as bytes first so the reported size is the wire size, then
     * decodes it as UTF-8 (`fatal: false`, because an error page may not be valid
     * UTF-8 and the playground should still show it).
     */
    function readResponse(response) {
        return response.arrayBuffer().then(function (buffer) {
            var bytes = new Uint8Array(buffer);
            return {
                response: response,
                byteLength: bytes.byteLength,
                text: decodeBytes(bytes)
            };
        });
    }

    function decodeBytes(bytes) {
        if (typeof TextDecoder === "function") {
            try {
                return new TextDecoder("utf-8", { fatal: false }).decode(bytes);
            } catch (error) {
                // fall through to the manual decode
            }
        }
        var out = "";
        for (var i = 0; i < bytes.length; i += 1) {
            out += String.fromCharCode(bytes[i]);
        }
        return out;
    }

    // ---------------------------------------------------------------- rendering

    /** Status class: 2xx ok, 3xx redirect, 4xx/5xx error, 1xx info. */
    function statusTone(status) {
        if (status >= 200 && status < 300) {
            return "ok";
        }
        if (status >= 300 && status < 400) {
            return "redirect";
        }
        if (status >= 400) {
            return "error";
        }
        return "info";
    }

    /** Pretty-prints JSON only when the payload really is JSON. */
    function presentBody(contentType, raw) {
        if (raw === "") {
            return "";
        }
        var looksJson = contentType.indexOf("json") !== -1;
        if (!looksJson) {
            return raw;
        }
        try {
            return JSON.stringify(JSON.parse(raw), null, 2);
        } catch (error) {
            // A JSON content type with a body that is not JSON is worth seeing raw.
            return raw;
        }
    }

    /** Hides every response part; used on selection change and on Clear. */
    function clearResponse() {
        setState(ui.responseState, "No request has been sent yet.", null);
        show(ui.responseFacts, false);
        show(ui.headersHead, false);
        show(ui.responseHeaders, false);
        show(ui.bodyHead, false);
        show(ui.responseBody, false);
        show(ui.responseTiming, true);
        text(ui.responseTiming, "");
        text(ui.statusPill, "");
        text(ui.factUrl, "");
        text(ui.bodySize, "");
        text(ui.responseBody, "");
        clearChildren(ui.responseHeaders);
    }

    /** Response headers, sorted, as a `dl` of text nodes. */
    function renderHeaders(headers) {
        var pairs = [];
        headers.forEach(function (value, key) {
            pairs.push({ key: key, value: value });
        });
        pairs.sort(function (a, b) {
            return a.key < b.key ? -1 : a.key > b.key ? 1 : 0;
        });
        pairs.forEach(function (pair) {
            var term = document.createElement("dt");
            text(term, pair.key + ":");
            var detail = document.createElement("dd");
            text(detail, pair.value);
            ui.responseHeaders.appendChild(term);
            ui.responseHeaders.appendChild(detail);
        });
        show(ui.headersHead, pairs.length > 0);
        show(ui.responseHeaders, pairs.length > 0);
    }

    function renderResponse(entry, url, result, started) {
        var response = result.response;
        var elapsed = nowMs() - started;
        var contentType = response.headers.get("content-type") || "unknown";
        var status = response.status;

        clearResponse();
        setState(ui.responseState, "", null);
        show(ui.responseState, false);

        ui.statusPill.setAttribute("data-tone", statusTone(status));
        text(ui.statusPill, status + (response.statusText ? " " + response.statusText : ""));
        text(ui.factUrl, entry.method + " " + url);
        show(ui.responseFacts, true);

        text(ui.responseTiming, formatMs(elapsed));
        text(ui.bodySize, plural(result.byteLength, "byte", "bytes") + " · content-type " + contentType);
        show(ui.bodyHead, true);

        renderHeaders(response.headers);

        var rendered = presentBody(contentType.toLowerCase(), result.text);
        if (rendered === "") {
            text(ui.responseBody, result.byteLength === 0 ? "(empty body)" : "(body could not be decoded as text)");
        } else if (rendered.length > MAX_RENDERED_BODY_CHARS) {
            text(ui.responseBody, rendered.slice(0, MAX_RENDERED_BODY_CHARS) + "\n… truncated, " + (rendered.length - MAX_RENDERED_BODY_CHARS) + " characters not shown.");
        } else {
            text(ui.responseBody, rendered);
        }
        show(ui.responseBody, true);

        if (status >= 400) {
            // The response is still rendered in full; the pill and the tone carry
            // the failure so a 4xx body (the API's own message) stays readable.
            setState(ui.responseState, "The service answered " + status + ".", "error");
            show(ui.responseState, true);
        }
    }

    function renderFailure(entry, url, error, started) {
        var elapsed = nowMs() - started;
        clearResponse();

        ui.statusPill.setAttribute("data-tone", "error");
        text(ui.statusPill, state.timedOut ? "timeout" : error && error.name === "AbortError" ? "cancelled" : "failed");
        text(ui.factUrl, entry.method + " " + url);
        show(ui.responseFacts, true);
        text(ui.responseTiming, formatMs(elapsed));

        var message;
        if (state.timedOut) {
            message = "No response within " + Math.round(REQUEST_TIMEOUT_MS / 1000) + "s; the request was aborted.";
        } else if (error && error.name === "AbortError") {
            message = "Request cancelled before the service answered.";
        } else {
            // A TypeError is what `fetch` rejects with for a refused connection, a
            // blocked redirect (`redirect: "error"`), a cross-origin answer, or a
            // CSP refusal - all of them "the request never produced a response".
            message =
                "The request never produced a response: " + describeError(error) +
                ". The service may be stopped, the URL may redirect outside this origin, or the request was blocked.";
        }
        setState(ui.responseState, message, "error");
        show(ui.responseState, true);
    }

    function formatMs(elapsed) {
        if (elapsed < 1000) {
            return Math.round(elapsed) + " ms";
        }
        return (elapsed / 1000).toFixed(2) + " s";
    }

    // ------------------------------------------------------------------- wiring

    ui.form.addEventListener("submit", function (event) {
        // The form exists for Enter-to-send; it must never navigate the page.
        event.preventDefault();
        send();
    });

    ui.addQuery.addEventListener("click", function () {
        addRow(ui.queryRows, "Query parameter name", "value", "", "");
    });

    ui.addHeader.addEventListener("click", function () {
        addRow(ui.headerRows, "Header name", "header value", "", "");
    });

    // One delegated listener covers rows that do not exist yet.
    [ui.queryRows, ui.headerRows].forEach(function (container) {
        container.addEventListener("click", function (event) {
            var target = event.target;
            if (!target || !target.classList.contains("remove-row")) {
                return;
            }
            var row = target.parentNode;
            if (row && row.parentNode === container) {
                container.removeChild(row);
            }
            // Always keep one blank row so the affordance cannot disappear.
            if (container.querySelectorAll(".kv-row").length === 0) {
                addRow(container, container === ui.queryRows ? "Query parameter name" : "Header name", "value", "", "");
            }
        });
    });

    ui.endpointSearch.addEventListener("input", renderList);
    ui.endpointSearch.addEventListener("search", renderList);

    // The failed-listing affordance re-runs the one automatic request, in place: no page
    // reload, so whatever the user already typed in the composer stays typed.
    ui.reloadEndpoints.addEventListener("click", function () {
        loadEndpoints();
    });

    ui.cancel.addEventListener("click", function () {
        abortInFlight();
    });

    ui.clear.addEventListener("click", function () {
        clearRequestError();
        clearResponse();
    });

    document.addEventListener("keydown", function (event) {
        if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
            event.preventDefault();
            send();
        } else if (event.key === "Escape" && state.busy) {
            event.preventDefault();
            abortInFlight();
        }
    });

    setBusy(false);
    loadEndpoints();
})();
