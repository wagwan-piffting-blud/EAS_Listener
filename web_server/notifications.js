/**
 * Edits apprise.yml: the list of URLs alerts are announced to. Shared by the configuration page
 * and first-run setup, which differ only in how they reach the API.
 *
 * New URLs are built from the service list the installed Apprise reports (`apprise --schema`), so
 * every service it supports can be added without knowing its URL format. Anything the builder does
 * not cover can be pasted as a URL. Every URL, saved or not, can be sent a test on its own.
 */
(function () {
    "use strict";

    function el(tag, attrs, ...children) {
        const node = document.createElement(tag);
        for (const [name, value] of Object.entries(attrs || {})) {
            if (value === undefined || value === null || value === false) continue;
            if (name === "class") node.className = value;
            else if (name === "text") node.textContent = value;
            else if (name.startsWith("on")) node.addEventListener(name.slice(2), value);
            else node.setAttribute(name, value === true ? "" : value);
        }
        for (const child of children.flat()) {
            if (child === null || child === undefined || child === false) continue;
            node.append(child instanceof Node ? child : document.createTextNode(String(child)));
        }
        return node;
    }

    const placeholders = (template) => [...template.matchAll(/\{(\w+)\}/g)].map((match) => match[1]);
    const schemeOf = (url) => (url.split("://")[0] || "").trim().toLowerCase();

    function maskUrl(url) {
        const [scheme, rest] = url.split(/:\/\/(.*)/s);
        if (rest === undefined) return "****";
        return rest.length <= 8 ? `${scheme}://****` : `${scheme}://****${rest.slice(-4)}`;
    }

    /** Apprise writes its patterns for Python; JavaScript spells named groups without the P. */
    function patternOf(spec) {
        if (!Array.isArray(spec.regex) || !spec.regex[0]) return null;
        try {
            return new RegExp(spec.regex[0].replace(/\(\?P</g, "(?<"), (spec.regex[1] || "").replace(/[^gimsuy]/g, ""));
        } catch (err) {
            return null;
        }
    }

    function create(options) {
        const request = options.request;
        const base = options.basePath;
        const onChange = options.onChange || (() => {});

        // Each target is { url, sources, events }; empty lists mean every alert.
        let saved = [];
        let targets = [];
        let otherLines = 0;
        let filePath = "";
        let catalog = null;
        let catalogError = null;
        let catalogPromise = null;
        const byScheme = new Map();
        const editing = new Set();

        const pathNote = el("code", {});
        const warning = el("p", { class: "cfg-note", hidden: true });
        const appriseNote = el("p", { class: "cfg-note", hidden: true });
        const list = el("div", { class: "ntf-list" });
        const empty = el("p", { class: "cfg-muted", text: "Nothing yet: alerts are not announced anywhere." });
        const builder = el("div", { class: "ntf-builder", hidden: true });
        const addButton = el("button", { type: "button", class: "cfg-mini", text: "Add a service", onclick: () => openCatalog() });
        const pasteButton = el("button", { type: "button", class: "cfg-mini", text: "Paste a URL", onclick: () => openPaste("") });

        const node = el("section", { class: "cfg-group ntf", id: "cfg-group-notifications", "data-group": "notifications" },
            el("div", { class: "cfg-group-head" },
                el("h2", { text: "Notifications" }),
                el("p", { class: "cfg-group-summary" },
                    "Where each alert is announced, kept in ", pathNote,
                    ". Discord webhooks are sent by the listener itself, with the recording attached; every other service goes through Apprise, which the listener installs when one is added. Route a URL to send it only some alerts: IPAWS to one webhook and NAAD to another, say. A saved change applies from the next alert.")),
            el("div", { class: "ntf-body" }, warning, appriseNote, empty, list,
                el("div", { class: "cfg-list-actions" }, addButton, pasteButton),
                builder));

        function changed() {
            renderList();
            onChange();
        }

        // ----- the list -----

        function serviceName(url) {
            const scheme = schemeOf(url);
            if (scheme === "discord") return "Discord";
            const service = byScheme.get(scheme);
            return service ? service.name : scheme || "Unknown";
        }

        async function sendTest(url, button, result) {
            const label = button.textContent;
            button.disabled = true;
            button.textContent = "Sending...";
            result.hidden = false;
            result.className = "ntf-result";
            result.textContent = "Sending a test message...";
            try {
                const response = await request(`${base}/test`, {
                    method: "POST",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ url }),
                });
                const outcome = await response.json().catch(() => null);
                if (!response.ok || !outcome) throw new Error(outcome && outcome.error ? outcome.error : `HTTP ${response.status}`);
                result.className = `ntf-result ${outcome.ok ? "is-ok" : "is-bad"}`;
                result.textContent = outcome.ok ? `${outcome.message} Check that it arrived.` : `Not sent: ${outcome.message}`;
            } catch (err) {
                result.className = "ntf-result is-bad";
                result.textContent = `Not sent: ${err.message}`;
            } finally {
                button.disabled = false;
                button.textContent = label;
            }
        }

        // ----- routes -----

        const SOURCES = [
            ["offair", "Off the air"],
            ["ipaws", "IPAWS"],
            ["wea", "IPAWS WEA"],
            ["naad", "NAAD (Alert Ready)"],
        ];
        const routing = new Set();

        function routeSummary(target) {
            const sources = target.sources.length
                ? target.sources.map((key) => (SOURCES.find(([k]) => k === key) || [key, key])[1]).join(", ")
                : "Every source";
            const events = target.events.length ? target.events.join(", ") : "every event";
            return target.sources.length || target.events.length ? `${sources} · ${events}` : "Every alert";
        }

        function parseEvents(text) {
            return [...new Set(text.split(/[\s,]+/).map((code) => code.trim().toUpperCase()).filter(Boolean))];
        }

        function routeEditor(target, index) {
            const boxes = SOURCES.map(([key, label]) => {
                const box = el("input", { type: "checkbox", value: key, checked: target.sources.length === 0 || target.sources.includes(key) });
                box.addEventListener("change", () => {
                    const ticked = boxes.filter((b) => b.checked).map((b) => b.value);
                    // All ticked means no restriction; none ticked would send nothing, so it is refused.
                    if (ticked.length === 0) {
                        box.checked = true;
                        return;
                    }
                    target.sources = ticked.length === SOURCES.length ? [] : ticked;
                    summary.textContent = routeSummary(target);
                    onChange();
                });
                return box;
            });
            const events = el("input", {
                type: "text",
                class: "cfg-input cfg-mono",
                placeholder: "Every event, or codes such as TOR, SVR, EAN",
                spellcheck: "false",
                autocomplete: "off",
                "aria-label": `Event codes for URL ${index + 1}`,
            });
            events.value = target.events.join(", ");
            const eventNote = el("p", { class: "ntf-result is-bad", hidden: true });
            events.addEventListener("input", () => {
                const codes = parseEvents(events.value);
                const bad = codes.filter((code) => !/^[A-Z0-9]{3}$/.test(code));
                eventNote.hidden = bad.length === 0;
                eventNote.textContent = bad.length ? `Not a SAME event code: ${bad.join(", ")}. They are three letters, such as TOR.` : "";
                target.events = codes;
                summary.textContent = routeSummary(target);
                onChange();
            });
            const summary = el("span", { class: "cfg-muted", text: routeSummary(target) });
            return el("div", { class: "ntf-route" },
                el("fieldset", { class: "ntf-route-sources" },
                    el("legend", { text: "Sources" }),
                    ...SOURCES.map(([key, label], i) => el("label", { class: "ntf-route-source" }, boxes[i], ` ${label}`))),
                el("label", { class: "ntf-route-events" }, el("span", { text: "Event codes" }), events),
                eventNote,
                el("p", { class: "cfg-help" }, "Sent only alerts from a ticked source whose event code is listed. Leave the codes empty for every event. ", summary));
        }

        function renderList() {
            empty.hidden = targets.length > 0;
            list.replaceChildren(...targets.map((target, index) => {
                const url = target.url;
                const result = el("p", { class: "ntf-result", hidden: true });
                const isEditing = editing.has(index);
                const isRouting = routing.has(index);
                let shown;
                if (isEditing) {
                    shown = el("input", { type: "text", class: "cfg-input cfg-mono", spellcheck: "false", autocomplete: "off", "aria-label": `URL ${index + 1}` });
                    shown.value = url;
                    shown.addEventListener("input", () => {
                        targets[index].url = shown.value.trim();
                        onChange();
                    });
                } else {
                    shown = el("code", { class: "ntf-url", text: maskUrl(url), title: "Hidden, since it holds the service's credentials" });
                }
                const test = el("button", { type: "button", class: "cfg-mini", text: "Send a test" });
                test.addEventListener("click", () => sendTest(targets[index].url, test, result));
                return el("div", { class: "ntf-row" },
                    el("div", { class: "cfg-list-row" },
                        el("span", { class: "ntf-service", text: serviceName(url) }),
                        shown,
                        el("button", {
                            type: "button",
                            class: "cfg-mini",
                            text: isEditing ? "Done" : "Edit",
                            onclick: () => {
                                if (isEditing) editing.delete(index);
                                else editing.add(index);
                                if (targets.some((t) => !t.url)) {
                                    targets = targets.filter((t) => t.url);
                                    editing.clear();
                                    routing.clear();
                                }
                                changed();
                            },
                        }),
                        el("button", {
                            type: "button",
                            class: "cfg-mini",
                            text: isRouting ? "Done" : "Route",
                            title: routeSummary(target),
                            "aria-expanded": isRouting ? "true" : "false",
                            onclick: () => {
                                if (isRouting) routing.delete(index);
                                else routing.add(index);
                                changed();
                            },
                        }),
                        test,
                        el("button", {
                            type: "button",
                            class: "cfg-icon",
                            title: "Remove",
                            "aria-label": `Remove ${serviceName(url)}`,
                            text: "×",
                            onclick: () => {
                                targets.splice(index, 1);
                                editing.clear();
                                routing.clear();
                                changed();
                            },
                        })),
                    isRouting ? routeEditor(target, index) : el("p", { class: "ntf-route-summary", text: routeSummary(target) }),
                    result);
            }));
        }

        function add(url) {
            targets.push({ url, sources: [], events: [] });
            closeBuilder();
            changed();
            if (schemeOf(url) !== "discord") loadCatalog();
        }

        // ----- the builder -----

        function closeBuilder() {
            builder.hidden = true;
            builder.replaceChildren();
            addButton.hidden = false;
            pasteButton.hidden = false;
        }

        function showBuilder(...children) {
            // replaceChildren would print a null as the text "null".
            builder.replaceChildren(...children.filter(Boolean));
            builder.hidden = false;
            addButton.hidden = true;
            pasteButton.hidden = true;
        }

        function loadCatalog() {
            if (!catalogPromise) {
                catalogPromise = (async () => {
                    try {
                        const response = await request(`${base}/services`);
                        const payload = await response.json().catch(() => null);
                        if (!response.ok || !payload || !Array.isArray(payload.services)) {
                            throw new Error(payload && payload.error ? payload.error : `HTTP ${response.status}`);
                        }
                        catalog = payload;
                        for (const service of catalog.services) {
                            for (const scheme of service.schemes) byScheme.set(String(scheme).toLowerCase(), service);
                        }
                        appriseNote.hidden = true;
                    } catch (err) {
                        catalogError = err.message;
                        appriseNote.textContent = `${catalogError} Only Discord webhooks can be sent until Apprise is installed. The listener fetches it itself when a service needs it; URLs for other services can still be pasted and saved.`;
                        appriseNote.hidden = false;
                        // Asked again next time: the fetch may have finished, or been retried.
                        catalogPromise = null;
                    }
                    renderList();
                })();
            }
            return catalogPromise;
        }

        function openPaste(initial) {
            const input = el("input", { type: "text", class: "cfg-input cfg-mono", placeholder: "service://...", spellcheck: "false", autocomplete: "off", "aria-label": "Apprise URL" });
            input.value = initial;
            const result = el("p", { class: "ntf-result", hidden: true });
            const test = el("button", { type: "button", class: "cfg-mini", text: "Send a test" });
            const valid = () => {
                const url = input.value.trim();
                if (!/^[a-z0-9+.-]+:\/\/\S+$/i.test(url)) {
                    result.hidden = false;
                    result.className = "ntf-result is-bad";
                    result.textContent = "That is not an Apprise URL. They look like service://..., for example tgram://bottoken/chatid.";
                    return null;
                }
                return url;
            };
            test.addEventListener("click", () => {
                const url = valid();
                if (url) sendTest(url, test, result);
            });
            showBuilder(
                el("h3", { text: "Paste a URL" }),
                el("p", { class: "cfg-help" }, "Any URL Apprise accepts. The formats are listed at ",
                    el("a", { href: "https://appriseit.com/services/", target: "_blank", rel: "noopener", text: "appriseit.com/services" }), "."),
                input,
                result,
                el("div", { class: "cfg-list-actions" },
                    test,
                    el("button", { type: "button", class: "cfg-mini cfg-primary-mini", text: "Add", onclick: () => {
                        const url = valid();
                        if (url) add(url);
                    } }),
                    el("button", { type: "button", class: "cfg-mini", text: "Cancel", onclick: closeBuilder })));
            input.focus();
        }

        async function openCatalog() {
            showBuilder(el("p", { class: "cfg-muted", text: "Asking Apprise which services it supports..." }));
            await loadCatalog();
            if (!catalog) {
                openPaste("");
                return;
            }

            const search = el("input", { type: "search", class: "cfg-input", placeholder: `Search ${catalog.services.length} services`, "aria-label": "Search services" });
            const results = el("div", { class: "ntf-catalog", role: "listbox", "aria-label": "Services" });
            function renderResults() {
                const query = search.value.trim().toLowerCase();
                const matches = catalog.services.filter((service) => !query
                    || service.name.toLowerCase().includes(query)
                    || service.schemes.some((scheme) => String(scheme).toLowerCase().includes(query)));
                results.replaceChildren(...matches.map((service) => el("button", {
                    type: "button",
                    class: "ntf-catalog-item",
                    role: "option",
                    onclick: () => openService(service),
                }, el("span", { text: service.name }), el("code", { text: service.schemes.map((scheme) => `${scheme}://`).join(" ") }))));
                if (matches.length === 0) {
                    results.append(el("p", { class: "cfg-muted", text: "No service matches. Paste its URL instead." }));
                }
            }
            search.addEventListener("input", renderResults);
            search.addEventListener("keydown", (event) => {
                if (event.key === "Enter") {
                    event.preventDefault();
                    const first = results.querySelector("button");
                    if (first) first.click();
                }
            });
            renderResults();
            showBuilder(
                el("h3", { text: "Add a service" }),
                search,
                results,
                el("div", { class: "cfg-list-actions" },
                    el("button", { type: "button", class: "cfg-mini", text: "Paste a URL instead", onclick: () => openPaste("") }),
                    el("button", { type: "button", class: "cfg-mini", text: "Cancel", onclick: closeBuilder })));
            search.focus();
        }

        function openService(service) {
            const templates = service.templates.map((template) => ({ template, keys: placeholders(template).filter((key) => key !== "schema") }));
            const tokens = service.tokens;
            const inTemplates = new Set(templates.flatMap((each) => each.keys));
            const inEvery = new Set([...inTemplates].filter((key) => templates.every((each) => each.keys.includes(key))));
            const listMembers = new Set(Object.values(tokens).flatMap((spec) => spec.group || []));
            const order = [];
            for (const each of templates) for (const key of each.keys) if (!order.includes(key)) order.push(key);
            const shown = order.filter((key) => tokens[key] && !(listMembers.has(key) && !inTemplates.has(key)));

            const values = {};
            const argValues = {};
            const problems = new Map();
            let showSecrets = false;

            const schemeSpec = tokens.schema || {};
            const schemes = (schemeSpec.values && schemeSpec.values.length ? schemeSpec.values : service.schemes).map(String);
            let scheme = schemes.includes(schemeSpec.default) ? schemeSpec.default
                : schemes.find((each) => service.secure_schemes.includes(each)) || schemes[0];

            const preview = el("code", { class: "ntf-preview" });
            const status = el("p", { class: "ntf-result" });
            const result = el("p", { class: "ntf-result", hidden: true });

            function tokenValue(key, raw) {
                const spec = tokens[key] || {};
                const value = raw.trim();
                if (key === "host" || key === "port") return value;
                if (key === "path") {
                    const segments = value.split("/").filter(Boolean).map(encodeURIComponent);
                    return segments.length ? `/${segments.join("/")}/` : "/";
                }
                if (String(spec.type).startsWith("list:")) {
                    const delimiter = (spec.delim && spec.delim[0]) || "/";
                    return value.split(/[\s,]+/).filter(Boolean).map(encodeURIComponent).join(delimiter);
                }
                return encodeURIComponent(value);
            }

            function build(masked) {
                const filled = (key) => (values[key] || "").trim() !== "";
                const usable = templates.filter((each) => each.keys.every(filled));
                if (usable.length === 0) {
                    const closest = templates
                        .map((each) => ({ each, missing: each.keys.filter((key) => !filled(key)) }))
                        .sort((a, b) => a.missing.length - b.missing.length)[0];
                    return { missing: closest ? closest.missing : [] };
                }
                const chosen = usable.reduce((best, each) => (each.keys.length > best.keys.length ? each : best));
                const unused = shown.filter((key) => filled(key) && !chosen.keys.includes(key));
                let url = chosen.template.replace(/\{(\w+)\}/g, (_, key) => {
                    if (key === "schema") return scheme;
                    if (masked && tokens[key] && tokens[key].private) return "****";
                    return tokenValue(key, values[key]);
                });
                const query = Object.entries(argValues)
                    .filter(([, value]) => value !== "")
                    .map(([key, value]) => {
                        const spec = service.args[key] || catalog.common_args[key] || {};
                        const shownValue = masked && spec.private ? "****" : encodeURIComponent(value);
                        return `${encodeURIComponent(key)}=${shownValue}`;
                    });
                if (query.length) url += `${url.includes("?") ? "&" : "?"}${query.join("&")}`;
                return { url, unused };
            }

            function label(key) {
                return (tokens[key] && tokens[key].name) || key;
            }

            function refresh() {
                const built = build(!showSecrets);
                const invalid = [...problems.values()];
                if (built.url) {
                    preview.textContent = built.url;
                    preview.classList.remove("is-incomplete");
                } else {
                    preview.textContent = `${scheme}://...`;
                    preview.classList.add("is-incomplete");
                }
                const notes = [];
                if (built.missing && built.missing.length) notes.push(`Still needed: ${built.missing.map(label).join(", ")}.`);
                if (built.unused && built.unused.length) notes.push(`Not part of any URL form together with the rest, so left out: ${built.unused.map(label).join(", ")}.`);
                notes.push(...invalid);
                status.textContent = notes.join(" ");
                status.className = `ntf-result${notes.length ? " is-bad" : ""}`;
                status.hidden = notes.length === 0;
                return built.url && invalid.length === 0 ? build(false).url : null;
            }

            function checkPattern(key, spec, input) {
                const pattern = patternOf(spec);
                const value = input.value.trim();
                if (pattern && value && !pattern.test(value)) problems.set(key, `${spec.name || key} does not look right for ${service.name}.`);
                else problems.delete(key);
                input.classList.toggle("is-invalid", problems.has(key));
            }

            function tokenField(key) {
                const spec = tokens[key];
                const type = String(spec.type || "string");
                const id = `ntf-${key}`;
                let control;
                if (type.startsWith("choice:")) {
                    control = el("select", { class: "cfg-input cfg-select", id });
                    if (!spec.required || spec.default === undefined) control.append(el("option", { value: "", text: "(not set)" }));
                    for (const value of spec.values || []) control.append(el("option", { value: String(value), text: String(value) }));
                    if (spec.default !== undefined && spec.default !== null) {
                        control.value = String(spec.default);
                        values[key] = control.value;
                    }
                    control.addEventListener("change", () => {
                        values[key] = control.value;
                        refresh();
                    });
                } else {
                    const numeric = type === "int" || type === "float";
                    control = el("input", {
                        type: numeric ? "number" : spec.private ? "password" : "text",
                        class: `cfg-input${spec.private ? " cfg-mono" : ""}`,
                        id,
                        min: spec.min,
                        max: spec.max,
                        step: type === "float" ? "any" : undefined,
                        placeholder: spec.default !== undefined && spec.default !== null ? String(spec.default) : "",
                        autocomplete: spec.private ? "new-password" : "off",
                        spellcheck: "false",
                    });
                    control.addEventListener("input", () => {
                        values[key] = control.value;
                        if (!type.startsWith("list:")) checkPattern(key, spec, control);
                        refresh();
                    });
                }

                const hints = [];
                if (type.startsWith("list:")) {
                    const members = (spec.group || []).map((member) => tokens[member]).filter(Boolean);
                    const described = members.map((member) => (member.prefix ? `${member.name} (starts with ${member.prefix})` : member.name));
                    hints.push(`One or more, separated by commas${described.length ? `: ${described.join(", ")}` : ""}.`);
                }
                if (spec.private) hints.push("Kept private: hidden in the list and the preview.");
                return el("div", { class: "ntf-field" },
                    el("label", { class: "cfg-label", for: id }, spec.name || key, inEvery.has(key) ? el("span", { class: "ntf-required", text: " (required)" }) : ""),
                    control,
                    hints.length ? el("p", { class: "cfg-help", text: hints.join(" ") }) : null);
            }

            function argField(key, spec) {
                const type = String(spec.type || "string");
                const id = `ntf-arg-${key}`;
                const fallback = spec.default === undefined || spec.default === null ? "" : String(spec.default);
                let control;
                if (type === "bool" || type.startsWith("choice:")) {
                    control = el("select", { class: "cfg-input cfg-select", id });
                    const defaultText = type === "bool" ? (spec.default ? "yes" : "no") : fallback;
                    control.append(el("option", { value: "", text: defaultText ? `Default (${defaultText})` : "Default" }));
                    const choices = type === "bool" ? ["yes", "no"] : (spec.values || []).map(String);
                    for (const value of choices) control.append(el("option", { value, text: value }));
                    control.addEventListener("change", () => {
                        argValues[key] = control.value;
                        refresh();
                    });
                } else {
                    const numeric = type === "int" || type === "float";
                    control = el("input", {
                        type: numeric ? "number" : spec.private ? "password" : "text",
                        class: "cfg-input",
                        id,
                        min: spec.min,
                        max: spec.max,
                        step: type === "float" ? "any" : undefined,
                        placeholder: fallback,
                        autocomplete: "off",
                        spellcheck: "false",
                    });
                    control.addEventListener("input", () => {
                        argValues[key] = control.value.trim();
                        refresh();
                    });
                }
                return el("div", { class: "ntf-field" },
                    el("label", { class: "cfg-label", for: id }, spec.name || key, " ", el("code", { class: "cfg-key", text: key })),
                    control);
            }

            const argEntries = (source) => Object.entries(source || {})
                .filter(([key, spec]) => !tokens[key] && spec && spec.name)
                .sort(([, a], [, b]) => String(a.name).localeCompare(String(b.name)));
            const ownArgs = argEntries(service.args);
            const commonArgs = argEntries(catalog.common_args);

            const schemeField = schemes.length > 1
                ? el("div", { class: "ntf-field" },
                    el("label", { class: "cfg-label", for: "ntf-scheme", text: "Connection" }),
                    (() => {
                        const select = el("select", { class: "cfg-input cfg-select", id: "ntf-scheme" });
                        for (const each of schemes) {
                            const secure = service.secure_schemes.includes(each);
                            select.append(el("option", { value: each, text: `${each}://${secure && schemes.length > 1 ? " (secure)" : ""}` }));
                        }
                        select.value = scheme;
                        select.addEventListener("change", () => {
                            scheme = select.value;
                            refresh();
                        });
                        return select;
                    })())
                : null;

            const reveal = el("label", { class: "cfg-toggle" },
                el("input", {
                    type: "checkbox",
                    onchange: (event) => {
                        showSecrets = event.target.checked;
                        refresh();
                    },
                }),
                " Show private values");

            const test = el("button", { type: "button", class: "cfg-mini", text: "Send a test" });
            test.addEventListener("click", () => {
                const url = refresh();
                if (url) sendTest(url, test, result);
            });

            const links = [
                service.setup_url ? el("a", { href: service.setup_url, target: "_blank", rel: "noopener", text: "How to set it up" }) : null,
                service.service_url ? el("a", { href: service.service_url, target: "_blank", rel: "noopener", text: "Service website" }) : null,
            ].filter(Boolean);

            showBuilder(
                el("div", { class: "ntf-service-head" },
                    el("h3", { text: service.name }),
                    links.length ? el("span", { class: "ntf-links" }, links.flatMap((link, index) => (index ? [" · ", link] : [link]))) : null),
                el("div", { class: "ntf-fields" }, schemeField, shown.map(tokenField)),
                ownArgs.length ? el("details", { class: "ntf-more" },
                    el("summary", { text: `More ${service.name} options (${ownArgs.length})` }),
                    el("div", { class: "ntf-fields" }, ownArgs.map(([key, spec]) => argField(key, spec)))) : null,
                commonArgs.length ? el("details", { class: "ntf-more" },
                    el("summary", { text: "Options every service has" }),
                    el("div", { class: "ntf-fields" }, commonArgs.map(([key, spec]) => argField(key, spec)))) : null,
                el("div", { class: "ntf-preview-row" }, el("span", { class: "cfg-muted", text: "URL:" }), preview, reveal),
                status,
                result,
                el("div", { class: "cfg-list-actions" },
                    test,
                    el("button", { type: "button", class: "cfg-mini cfg-primary-mini", text: "Add", onclick: () => {
                        const url = refresh();
                        if (url) add(url);
                        else status.scrollIntoView({ block: "nearest" });
                    } }),
                    el("button", { type: "button", class: "cfg-mini", text: "Other services", onclick: openCatalog }),
                    el("button", { type: "button", class: "cfg-mini", text: "Cancel", onclick: closeBuilder })));
            refresh();
            const first = builder.querySelector(".ntf-fields input, .ntf-fields select");
            if (first) first.focus();
        }

        // ----- loading and saving -----

        async function load() {
            const response = await request(base);
            const payload = await response.json().catch(() => null);
            if (!response.ok || !payload) throw new Error(payload && payload.error ? payload.error : `HTTP ${response.status}`);
            filePath = payload.path;
            pathNote.textContent = filePath;
            saved = copyTargets(payload.targets || []);
            targets = copyTargets(saved);
            otherLines = payload.other_lines || 0;
            warning.textContent = `${filePath} also has ${otherLines} ${otherLines === 1 ? "item" : "items"} that ${otherLines === 1 ? "is" : "are"} not a URL (YAML keys or tags, say). Saving from here keeps only the URLs and their routes.`;
            warning.hidden = otherLines === 0;
            editing.clear();
            routing.clear();
            renderList();
            // The service list needs Apprise, which the listener fetches when asked for it; a
            // Discord-only setup is not made to download it just by opening the page.
            if (targets.some((target) => schemeOf(target.url) !== "discord")) loadCatalog();
        }

        function copyTargets(list) {
            return list.map((target) => ({
                url: target.url,
                sources: (target.sources || []).slice(),
                events: (target.events || []).slice(),
            }));
        }

        function current() {
            return targets
                .map((target) => ({ url: target.url.trim(), sources: target.sources.slice(), events: target.events.slice() }))
                .filter((target) => target.url);
        }

        function dirty() {
            return JSON.stringify(current()) !== JSON.stringify(saved);
        }

        /** `extra` goes in the request body alongside the URLs; setup names the file this way. */
        async function save(extra) {
            const response = await request(base, {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(Object.assign({ targets: current() }, extra || {})),
            });
            const result = await response.json().catch(() => null);
            if (!response.ok || !result || !result.ok) {
                return { ok: false, error: result && result.error ? result.error : `HTTP ${response.status}` };
            }
            saved = current();
            targets = copyTargets(saved);
            otherLines = 0;
            warning.hidden = true;
            editing.clear();
            routing.clear();
            renderList();
            return result;
        }

        function revert() {
            targets = copyTargets(saved);
            editing.clear();
            routing.clear();
            closeBuilder();
            renderList();
        }

        function change() {
            if (!dirty()) return null;
            const count = (n) => `${n} ${n === 1 ? "URL" : "URLs"}`;
            const routed = (list) => list.filter((t) => t.sources.length || t.events.length).length;
            const describe = (list) => {
                const n = routed(list);
                return n ? `${count(list.length)}, ${n} routed` : count(list.length);
            };
            return { key: "apprise.yml", before: describe(saved), after: describe(current()) };
        }

        function setFilter(filter) {
            const query = (filter.query || "").trim().toLowerCase();
            const haystack = ["notifications apprise discord route routing ipaws naad wea", ...targets.map((t) => serviceName(t.url))].join(" ").toLowerCase();
            node.hidden = Boolean((query && !haystack.includes(query)) || (filter.onlySet && targets.length === 0));
        }

        renderList();
        return { node, load, save, dirty, revert, change, setFilter, count: () => current().length };
    }

    window.NotificationEditor = { create };
})();
