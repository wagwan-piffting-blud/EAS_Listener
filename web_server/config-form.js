/**
 * Renders config.json as a form, built from the schema GET /api/config/schema returns.
 *
 * The draft is config.json itself, as an object: an input writes its key, and "Use default"
 * deletes it so the listener's own default applies instead of a copy of it. Keys the schema does
 * not describe are left alone, so opening and saving the form never loses anything written by
 * hand. Used by both the configuration editor and first-run setup.
 */
(function () {
    "use strict";

    const hasOwn = (object, key) => Object.prototype.hasOwnProperty.call(object, key);
    const clone = (value) => (value === undefined ? undefined : JSON.parse(JSON.stringify(value)));
    const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
    const LIST_TYPES = new Set(["string_list", "stream_list", "cap_endpoints", "filters"]);

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

    function truthy(text) {
        return ["1", "true", "yes", "on"].includes(String(text).trim().toLowerCase());
    }

    /** A short, human form of a value, for defaults and the list of changes. */
    function describe(value) {
        if (value === null || value === undefined) return "not set";
        if (typeof value === "boolean") return value ? "on" : "off";
        if (typeof value === "string") return value === "" ? "empty" : value;
        if (Array.isArray(value)) {
            if (value.length === 0) return "none";
            return value
                .map((item) => (item && typeof item === "object" ? item.name || item.url || JSON.stringify(item) : String(item)))
                .join(", ");
        }
        if (typeof value === "object") {
            const count = Object.keys(value).length;
            return count === 0 ? "none" : `${count} ${count === 1 ? "entry" : "entries"}`;
        }
        return String(value);
    }

    let sameLookup = null;

    function create(options) {
        const schema = options.schema;
        const fields = schema.fields;
        const byKey = new Map(fields.map((field) => [field.key, field]));
        const confirmKeys = new Set(options.confirmSecrets || []);

        let original = clone(options.value) || {};
        let draft = clone(original);
        let filter = { query: "", onlySet: false };

        const views = new Map();
        const groupSections = new Map();
        const localErrors = new Map();
        const serverErrors = new Map();
        const confirmValues = new Map();
        const touched = new Set();

        // ----- values -----

        function isSet(key) {
            return hasOwn(draft, key) && draft[key] !== null;
        }

        function coerceEnvironment(field, text) {
            if (field.type === "bool") return truthy(text);
            if (field.type === "integer") {
                const number = Number(text);
                return Number.isFinite(number) ? number : text;
            }
            if (LIST_TYPES.has(field.type)) {
                try {
                    const parsed = JSON.parse(text);
                    if (Array.isArray(parsed)) return parsed;
                } catch (err) {
                    // Not JSON: shown as written.
                }
            }
            return text;
        }

        function environmentValue(field) {
            const env = field.environment;
            if (!env || env.value === null || env.value === undefined) return undefined;
            return coerceEnvironment(field, env.value);
        }

        /** What applies when config.json leaves the key out. */
        function fallback(field) {
            const fromEnv = environmentValue(field);
            return fromEnv === undefined ? field.default : fromEnv;
        }

        function fallbackIsEnvironment(field) {
            return environmentValue(field) !== undefined;
        }

        function environmentWins(field) {
            const env = field.environment;
            return !!env && (schema.precedence === "environment" || env.forced);
        }

        /** What the listener will actually use. */
        function effective(field) {
            if (environmentWins(field) && environmentValue(field) !== undefined) return environmentValue(field);
            return isSet(field.key) ? draft[field.key] : fallback(field);
        }

        function visible(field) {
            let parentKey = field.requires;
            while (parentKey) {
                const parent = byKey.get(parentKey);
                if (!parent || !effective(parent)) return false;
                parentKey = parent.requires;
            }
            return true;
        }

        function isChanged(key) {
            return hasOwn(original, key) !== hasOwn(draft, key) || !same(original[key], draft[key]);
        }

        function set(key, value) {
            // Turning something on and back off must not leave the default written into the file.
            const field = byKey.get(key);
            if (field && !field.setup_required && !hasOwn(original, key) && same(value, fallback(field))) {
                unset(key);
                return;
            }
            draft[key] = value;
            serverErrors.delete(key);
            changed();
        }

        function unset(key) {
            delete draft[key];
            serverErrors.delete(key);
            changed();
        }

        function changed() {
            refresh();
            if (options.onChange) options.onChange(api);
        }

        function setLocalError(key, message) {
            if (message) localErrors.set(key, message);
            else localErrors.delete(key);
        }

        // ----- controls -----

        function idFor(field) {
            return `cfg-${field.key}`;
        }

        function setText(field, text) {
            if (text.trim() === "") {
                // An explicit "" already in the file is kept rather than turned into a removal.
                if (hasOwn(original, field.key) && original[field.key] === "") set(field.key, "");
                else unset(field.key);
            } else {
                set(field.key, text);
            }
        }

        function placeholderFor(field) {
            const value = fallback(field);
            if (typeof value === "string" && value !== "") return value;
            if (typeof value === "number") return String(value);
            return field.placeholder || "";
        }

        function boolControl(field) {
            const input = el("input", { type: "checkbox", id: idFor(field), role: "switch" });
            input.checked = !!(isSet(field.key) ? draft[field.key] : fallback(field));
            input.addEventListener("change", () => set(field.key, input.checked));
            return el("label", { class: "cfg-switch", title: field.label }, input, el("span", { class: "cfg-switch-track" }));
        }

        function textControl(field, mono) {
            const input = el("input", {
                type: "text",
                id: idFor(field),
                class: mono ? "cfg-input cfg-mono" : "cfg-input",
                placeholder: placeholderFor(field),
                spellcheck: "false",
                autocomplete: "off",
            });
            input.value = isSet(field.key) ? String(draft[field.key]) : "";
            input.addEventListener("input", () => setText(field, input.value));
            return input;
        }

        function secretControl(field) {
            const input = el("input", {
                type: "password",
                id: idFor(field),
                class: "cfg-input",
                autocomplete: "new-password",
                spellcheck: "false",
            });
            input.value = isSet(field.key) ? String(draft[field.key]) : "";
            input.addEventListener("input", () => setText(field, input.value));

            const inputs = [input];
            const row = el("div", { class: "cfg-row" }, input);

            if (confirmKeys.has(field.key)) {
                const confirm = el("input", {
                    type: "password",
                    class: "cfg-input",
                    autocomplete: "new-password",
                    placeholder: "Type it again",
                    "aria-label": `${field.label}, again`,
                });
                confirm.value = confirmValues.get(field.key) ?? input.value;
                confirmValues.set(field.key, confirm.value);
                confirm.addEventListener("input", () => {
                    confirmValues.set(field.key, confirm.value);
                    refresh();
                });
                inputs.push(confirm);
                row.append(confirm);
            }

            const reveal = el("button", { type: "button", class: "cfg-mini", text: "Show" });
            reveal.addEventListener("click", () => {
                const hidden = input.type === "password";
                for (const each of inputs) each.type = hidden ? "text" : "password";
                reveal.textContent = hidden ? "Hide" : "Show";
            });
            row.append(reveal);
            return row;
        }

        function integerControl(field) {
            const input = el("input", {
                type: "number",
                id: idFor(field),
                class: "cfg-input cfg-narrow",
                min: field.min,
                max: field.max,
                step: "1",
                placeholder: placeholderFor(field),
            });
            input.value = isSet(field.key) ? String(draft[field.key]) : "";
            input.addEventListener("input", () => {
                const text = input.value.trim();
                if (input.validity.badInput) {
                    setLocalError(field.key, `Enter a whole number from ${field.min} to ${field.max}.`);
                    refresh();
                    return;
                }
                if (text === "") {
                    setLocalError(field.key, null);
                    unset(field.key);
                    return;
                }
                const number = Number(text);
                if (!Number.isInteger(number) || number < field.min || number > field.max) {
                    setLocalError(field.key, `Enter a whole number from ${field.min} to ${field.max}.`);
                    refresh();
                    return;
                }
                setLocalError(field.key, null);
                set(field.key, number);
            });
            return input;
        }

        function choiceControl(field) {
            const select = el("select", { id: idFor(field), class: "cfg-input cfg-select" });
            const fallbackValue = fallback(field);
            const known = field.options.find((option) => option.value === String(fallbackValue));
            const fallbackLabel = known ? known.label : describe(fallbackValue);
            const source = fallbackIsEnvironment(field) ? "environment" : "default";
            select.append(el("option", { value: "", text: `${fallbackLabel} (${source})` }));
            for (const option of field.options) {
                select.append(el("option", { value: option.value, text: option.label }));
            }

            const current = isSet(field.key) ? String(draft[field.key]) : "";
            if (current && !field.options.some((option) => option.value === current)) {
                select.append(el("option", { value: current, text: `${current} (as written)` }));
            }
            select.value = current;
            select.addEventListener("change", () => {
                if (select.value === "") unset(field.key);
                else set(field.key, select.value);
            });
            return select;
        }

        function timeZoneControl(field) {
            const listId = `${idFor(field)}-zones`;
            const zones = new Set(field.options);
            const browserZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
            const input = el("input", {
                type: "text",
                id: idFor(field),
                class: "cfg-input",
                list: listId,
                placeholder: placeholderFor(field),
                autocomplete: "off",
                spellcheck: "false",
            });
            input.value = isSet(field.key) ? String(draft[field.key]) : "";

            let useBrowserZone = null;

            function apply() {
                const text = input.value.trim();
                if (useBrowserZone) useBrowserZone.hidden = text === browserZone;
                if (text === "") {
                    setLocalError(field.key, null);
                    unset(field.key);
                } else if (zones.has(text)) {
                    setLocalError(field.key, null);
                    set(field.key, text);
                } else {
                    setLocalError(field.key, "Pick a time zone from the list, such as America/Chicago.");
                    refresh();
                }
            }
            input.addEventListener("input", apply);
            input.addEventListener("blur", () => {
                touched.add(field.key);
                refresh();
            });

            const row = el("div", { class: "cfg-row" }, input, el("datalist", { id: listId }, field.options.map((zone) => el("option", { value: zone }))));
            if (browserZone && zones.has(browserZone)) {
                useBrowserZone = el("button", {
                    type: "button",
                    class: "cfg-mini",
                    text: `Use ${browserZone}`,
                    hidden: input.value === browserZone,
                    onclick: () => {
                        input.value = browserZone;
                        apply();
                    },
                });
                row.append(useBrowserZone);
            }
            return row;
        }

        function listRow(className, inputs, onRemove, extraButtons) {
            const remove = el("button", { type: "button", class: "cfg-icon", title: "Remove", "aria-label": "Remove", text: "×" });
            const row = el("div", { class: `cfg-list-row ${className}` }, inputs, extraButtons || [], remove);
            remove.addEventListener("click", () => {
                row.remove();
                onRemove();
            });
            return row;
        }

        function stringListControl(field) {
            const rows = el("div", { class: "cfg-list-rows" });

            function commit() {
                const values = [...rows.querySelectorAll("input")].map((input) => input.value.trim()).filter(Boolean);
                if (values.length === 0) unset(field.key);
                else set(field.key, values);
            }

            function addRow(value, focus) {
                const input = el("input", { type: "text", class: "cfg-input cfg-mono", placeholder: field.placeholder || "", spellcheck: "false", autocomplete: "off" });
                input.value = value;
                input.addEventListener("input", commit);
                rows.append(listRow("", input, commit));
                if (focus) input.focus();
            }

            const current = isSet(field.key) && Array.isArray(draft[field.key]) ? draft[field.key] : [];
            current.forEach((value) => addRow(String(value)));

            const actions = el("div", { class: "cfg-list-actions" }, el("button", { type: "button", class: "cfg-mini", text: "Add", onclick: () => addRow("", true) }));
            const defaults = fallback(field);
            if (Array.isArray(defaults) && defaults.length > 0) {
                actions.append(el("button", {
                    type: "button",
                    class: "cfg-mini",
                    text: "Copy in the default",
                    onclick: () => {
                        defaults.forEach((value) => addRow(String(value)));
                        commit();
                    },
                }));
            }
            return el("div", { class: "cfg-list" }, rows, actions);
        }

        function streamListControl(field) {
            const nicknamesKey = field.nicknames_key;
            const nwrKey = field.nwr_key;
            const rows = el("div", { class: "cfg-list-rows" });
            const current = isSet(field.key) && Array.isArray(draft[field.key]) ? draft[field.key].map(String) : [];
            const nicknames = draft[nicknamesKey] && typeof draft[nicknamesKey] === "object" ? draft[nicknamesKey] : {};
            // Without the key every stream is watched for the 1050 Hz tone, so that is what the
            // boxes show: they start ticked, and the key is only written once one is cleared.
            const nwrDeclared = isSet(nwrKey) && Array.isArray(draft[nwrKey]);
            const nwrMarked = nwrDeclared ? draft[nwrKey].map(String) : [];

            // Nicknames for streams that are not rows here (set through the environment, say) are kept.
            const unrelated = Object.fromEntries(Object.entries(nicknames).filter(([url]) => !current.includes(url)));
            const unrelatedNwr = nwrMarked.filter((url) => !current.includes(url));

            function commit() {
                const entries = [...rows.children].map((row) => {
                    const [url, nick] = row.querySelectorAll('input[type="text"]');
                    const nwr = row.querySelector('input[type="checkbox"]');
                    return { url: url.value.trim(), nick: nick.value.trim(), nwr: nwr.checked };
                }).filter((entry) => entry.url);

                if (entries.length === 0) unset(field.key);
                else set(field.key, entries.map((entry) => entry.url));

                const mapping = Object.assign({}, unrelated);
                for (const entry of entries) {
                    if (entry.nick) mapping[entry.url] = entry.nick;
                }
                if (Object.keys(mapping).length === 0) {
                    if (hasOwn(draft, nicknamesKey)) unset(nicknamesKey);
                } else if (!same(mapping, draft[nicknamesKey])) {
                    set(nicknamesKey, mapping);
                }

                // Every row ticked is what an absent key already means, so it is left out unless
                // config.json named these streams itself.
                const marked = unrelatedNwr.concat(entries.filter((entry) => entry.nwr).map((entry) => entry.url));
                if (!nwrDeclared && entries.every((entry) => entry.nwr) && unrelatedNwr.length === 0) {
                    if (hasOwn(draft, nwrKey)) unset(nwrKey);
                } else if (!same(marked, draft[nwrKey])) {
                    set(nwrKey, marked);
                }
            }

            function addRow(url, nick, nwr, focus) {
                const urlInput = el("input", { type: "text", class: "cfg-input cfg-mono", placeholder: "https://stream.example.com/live.mp3", spellcheck: "false", autocomplete: "off", "aria-label": "Stream URL" });
                const nickInput = el("input", { type: "text", class: "cfg-input cfg-nick", placeholder: "Nickname (optional)", autocomplete: "off", "aria-label": "Nickname" });
                const nwrInput = el("input", { type: "checkbox", class: "cfg-stream-nwr-box", "aria-label": "NOAA Weather Radio stream" });
                urlInput.value = url;
                nickInput.value = nick;
                nwrInput.checked = nwr;
                urlInput.addEventListener("input", commit);
                nickInput.addEventListener("input", commit);
                nwrInput.addEventListener("change", commit);
                const nwrLabel = el("label", { class: "cfg-stream-nwr", title: "Listen for the 1050 Hz NOAA Weather Radio tone on this stream" }, nwrInput, el("span", { text: "NWR" }));
                rows.append(listRow("cfg-stream-row", [urlInput, nickInput], commit, nwrLabel));
                if (focus) urlInput.focus();
            }

            current.forEach((url) => addRow(url, nicknames[url] || "", !nwrDeclared || nwrMarked.includes(url)));

            const actions = el("div", { class: "cfg-list-actions" }, el("button", { type: "button", class: "cfg-mini", text: "Add stream", onclick: () => addRow("", "", !nwrDeclared, true) }));
            const defaults = fallback(field);
            if (Array.isArray(defaults) && defaults.length > 0) {
                const noun = fallbackIsEnvironment(field) ? "the environment's streams" : "the sample stream";
                actions.append(el("button", {
                    type: "button",
                    class: "cfg-mini",
                    text: `Use ${noun}`,
                    onclick: () => {
                        defaults.forEach((url) => addRow(String(url), "", !nwrDeclared));
                        commit();
                    },
                }));
            }
            return el("div", { class: "cfg-list" }, rows, actions);
        }

        function loadSameLookup() {
            if (!sameLookup) {
                sameLookup = options.fetchJson("/api/same-us")
                    .then((payload) => payload && payload.SAME ? payload.SAME : {})
                    .catch((err) => {
                        console.warn("SAME location names are unavailable:", err);
                        return {};
                    });
            }
            return sameLookup;
        }

        function locationName(same, code) {
            if (code === "000000") return "Entire United States";
            return same[code.slice(1).replace(/^0+/, "")] || "";
        }

        function fipsControl(field) {
            let codes = isSet(field.key)
                ? String(draft[field.key]).split(",").map((code) => code.trim()).filter(Boolean)
                : [];
            let names = {};
            const optionValues = new Map();

            const listId = `${idFor(field)}-places`;
            const chips = el("div", { class: "cfg-chips" });
            const datalist = el("datalist", { id: listId });
            const input = el("input", {
                type: "text",
                id: idFor(field),
                class: "cfg-input",
                list: listId,
                placeholder: "Type a county or a 6-digit code",
                autocomplete: "off",
                spellcheck: "false",
            });

            function commit() {
                if (codes.length === 0) unset(field.key);
                else set(field.key, codes.join(","));
            }

            function render() {
                chips.replaceChildren(...codes.map((code) => {
                    const name = locationName(names, code);
                    return el("span", { class: "cfg-chip" },
                        el("code", { text: code }),
                        name ? ` ${name}` : "",
                        el("button", {
                            type: "button",
                            class: "cfg-chip-remove",
                            "aria-label": `Remove ${code}`,
                            text: "×",
                            onclick: () => {
                                codes = codes.filter((each) => each !== code);
                                render();
                                commit();
                            },
                        }));
                }));
                if (codes.length === 0) chips.append(el("span", { class: "cfg-muted", text: "Every location is watched." }));
            }

            function add(text) {
                const value = text.trim();
                if (!value) return;
                const match = optionValues.get(value) || (value.match(/^(\d{5,6})$/) || [])[1];
                if (!match) {
                    setLocalError(field.key, "Pick a place from the list, or type its 6-digit SAME code.");
                    touched.add(field.key);
                    refresh();
                    return;
                }
                setLocalError(field.key, null);
                const code = match.padStart(6, "0");
                if (!codes.includes(code)) codes.push(code);
                input.value = "";
                render();
                commit();
            }

            input.addEventListener("input", () => {
                if (optionValues.has(input.value)) add(input.value);
                else if (localErrors.has(field.key)) {
                    setLocalError(field.key, null);
                    refresh();
                }
            });
            input.addEventListener("keydown", (event) => {
                if (event.key === "Enter") {
                    event.preventDefault();
                    add(input.value);
                }
            });

            loadSameLookup().then((same) => {
                names = same;
                const entries = [["000000", "Entire United States"]];
                for (const [key, name] of Object.entries(same)) {
                    entries.push([`0${key.padStart(5, "0")}`, name]);
                }
                const fragment = document.createDocumentFragment();
                for (const [code, name] of entries) {
                    const label = `${name} (${code})`;
                    optionValues.set(label, code);
                    fragment.append(el("option", { value: label }));
                }
                datalist.replaceChildren(fragment);
                render();
            });

            render();
            const addButton = el("button", { type: "button", class: "cfg-mini", text: "Add", onclick: () => add(input.value) });
            return el("div", { class: "cfg-fips" }, chips, el("div", { class: "cfg-row" }, input, addButton, datalist));
        }

        function capEndpointsControl(field) {
            const rows = el("div", { class: "cfg-list-rows" });
            const current = isSet(field.key) && Array.isArray(draft[field.key]) ? draft[field.key] : [];

            function commit() {
                const entries = [...rows.children].map((row) => {
                    const [name, url] = row.querySelectorAll("input");
                    return { name: name.value.trim(), url: url.value.trim() };
                }).filter((entry) => entry.url);
                if (entries.length === 0) unset(field.key);
                else set(field.key, entries.map((entry) => (entry.name ? entry : entry.url)));
            }

            function urls() {
                return [...rows.children].map((row) => row.querySelectorAll("input")[1].value.trim());
            }

            function addRow(name, url, focus) {
                const nameInput = el("input", { type: "text", class: "cfg-input cfg-nick", placeholder: "Name (optional)", autocomplete: "off", "aria-label": "Feed name" });
                const urlInput = el("input", { type: "text", class: "cfg-input cfg-mono", placeholder: "https://apps.fema.gov/IPAWSOPEN_EAS_SERVICE/rest/feed", spellcheck: "false", autocomplete: "off", "aria-label": "Feed URL" });
                nameInput.value = name;
                urlInput.value = url;
                nameInput.addEventListener("input", commit);
                urlInput.addEventListener("input", commit);
                rows.append(listRow("cfg-endpoint-row", [nameInput, urlInput], commit));
                if (focus) nameInput.focus();
            }

            for (const entry of current) {
                if (typeof entry === "string") addRow("", entry);
                else if (entry && typeof entry === "object") addRow(entry.name || "", entry.url || "");
            }

            const actions = el("div", { class: "cfg-list-actions" }, el("button", { type: "button", class: "cfg-mini", text: "Add feed", onclick: () => addRow("", "", true) }));
            for (const preset of field.presets || []) {
                actions.append(el("button", {
                    type: "button",
                    class: "cfg-mini",
                    text: `Add ${preset.label}`,
                    onclick: () => {
                        const present = new Set(urls());
                        for (const endpoint of preset.endpoints) {
                            if (!present.has(endpoint.url)) addRow(endpoint.name || "", endpoint.url);
                        }
                        commit();
                    },
                }));
            }
            return el("div", { class: "cfg-list" }, rows, actions);
        }

        function filtersControl(field) {
            const rows = el("div", { class: "cfg-list-rows" });
            const current = isSet(field.key) && Array.isArray(draft[field.key]) ? draft[field.key] : [];
            const extras = new WeakMap();

            function commit() {
                let incomplete = false;
                const rules = [...rows.children].map((row) => {
                    const [name, codes] = row.querySelectorAll("input");
                    const action = row.querySelector("select").value;
                    const eventCodes = codes.value.split(/[\s,]+/).map((code) => code.trim()).filter(Boolean)
                        .map((code) => (code === "*" ? code : code.toUpperCase()));
                    if (!name.value.trim() || eventCodes.length === 0) incomplete = true;
                    return Object.assign({}, extras.get(row) || {}, { name: name.value.trim(), event_codes: eventCodes, action });
                });
                setLocalError(field.key, incomplete ? "Every rule needs a name and at least one event code." : null);
                const kept = rules.filter((rule) => rule.name || rule.event_codes.length);
                if (kept.length === 0) unset(field.key);
                else set(field.key, kept);
            }

            function move(row, offset) {
                const siblings = [...rows.children];
                const index = siblings.indexOf(row);
                const target = siblings[index + offset];
                if (!target) return;
                if (offset < 0) rows.insertBefore(row, target);
                else rows.insertBefore(target, row);
                commit();
            }

            function addRow(rule, focus) {
                const name = el("input", { type: "text", class: "cfg-input cfg-nick", placeholder: "Rule name", autocomplete: "off", "aria-label": "Rule name" });
                const codes = el("input", { type: "text", class: "cfg-input cfg-mono", placeholder: "TOR, SVR or *", spellcheck: "false", autocomplete: "off", "aria-label": "Event codes" });
                const action = el("select", { class: "cfg-input cfg-select", "aria-label": "Action" },
                    field.actions.map((option) => el("option", { value: option.value, text: option.label })));
                name.value = rule.name || "";
                codes.value = Array.isArray(rule.event_codes) ? rule.event_codes.join(", ") : "";
                action.value = field.actions.some((option) => option.value === rule.action) ? rule.action : "relay";
                name.addEventListener("input", commit);
                codes.addEventListener("input", commit);
                action.addEventListener("change", commit);

                const up = el("button", { type: "button", class: "cfg-icon", title: "Move up", "aria-label": "Move up", text: "↑" });
                const down = el("button", { type: "button", class: "cfg-icon", title: "Move down", "aria-label": "Move down", text: "↓" });
                const row = listRow("cfg-filter-row", [name, codes, action], commit, [up, down]);
                up.addEventListener("click", () => move(row, -1));
                down.addEventListener("click", () => move(row, 1));

                const extra = Object.assign({}, rule);
                delete extra.name;
                delete extra.event_codes;
                delete extra.action;
                extras.set(row, extra);
                rows.append(row);
                if (focus) name.focus();
            }

            current.forEach((rule) => addRow(rule && typeof rule === "object" ? rule : {}));

            const actions = el("div", { class: "cfg-list-actions" }, el("button", {
                type: "button",
                class: "cfg-mini",
                text: "Add rule",
                onclick: () => addRow({ action: "relay" }, true),
            }));
            return el("div", { class: "cfg-list" }, rows, actions);
        }

        function buildControl(field) {
            switch (field.type) {
                case "bool": return boolControl(field);
                case "secret": return secretControl(field);
                case "integer": return integerControl(field);
                case "path": return textControl(field, true);
                case "choice": return choiceControl(field);
                case "time_zone": return timeZoneControl(field);
                case "string_list": return stringListControl(field);
                case "fips_list": return fipsControl(field);
                case "stream_list": return streamListControl(field);
                case "cap_endpoints": return capEndpointsControl(field);
                case "filters": return filtersControl(field);
                default: return textControl(field, false);
            }
        }

        // ----- field chrome -----

        function defaultText(field) {
            if (field.type === "secret" || field.type === "managed") return null;
            return el("span", { class: "cfg-default" }, " Default: ", el("code", { text: describe(field.default) }), ".");
        }

        function buildField(field) {
            if (field.type === "managed") return null;

            const label = el("label", { class: "cfg-label", for: idFor(field), text: field.label });
            const badges = el("span", { class: "cfg-badges" });
            const reset = el("button", {
                type: "button",
                class: "cfg-reset",
                text: "Use default",
                title: "Remove this key from config.json, so the default applies",
            });
            const head = el("div", { class: "cfg-field-head" }, label, el("code", { class: "cfg-key", text: field.key }), badges, reset);
            const control = el("div", { class: "cfg-control" });
            const help = el("p", { class: "cfg-help" }, field.help, defaultText(field));
            const note = el("p", { class: "cfg-note", hidden: true });
            const annotation = el("p", { class: "cfg-annotation", hidden: true });
            const error = el("p", { class: "cfg-error", role: "alert", hidden: true });

            const root = el("div", { class: `cfg-field cfg-type-${field.type}`, "data-key": field.key });
            if (field.type === "bool") head.prepend(control);
            root.append(head);
            if (field.type !== "bool") root.append(control);
            root.append(help, note, annotation, error);

            const view = { field, root, control, badges, reset, note, annotation, error };
            view.rebuild = () => control.replaceChildren(buildControl(field));
            reset.addEventListener("click", () => {
                setLocalError(field.key, null);
                confirmValues.delete(field.key);
                delete draft[field.key];
                if (field.nicknames_key) delete draft[field.nicknames_key];
                if (field.nwr_key) delete draft[field.nwr_key];
                view.rebuild();
                changed();
            });
            root.addEventListener("focusout", () => {
                if (!touched.has(field.key)) {
                    touched.add(field.key);
                    refreshView(view);
                }
            });

            view.rebuild();
            views.set(field.key, view);
            return root;
        }

        function badge(text, kind, title) {
            return el("span", { class: `cfg-badge cfg-badge-${kind}`, text, title });
        }

        function environmentNote(field) {
            const env = field.environment;
            if (!env) return "";
            const shown = env.value === null ? "a hidden value" : `"${env.value}"`;
            if (env.forced) {
                return `The container sets ${field.key} to ${shown} when it starts, which overrides config.json.`;
            }
            if (schema.precedence === "environment") {
                return `The environment variable ${field.key} is set to ${shown}. It overrides config.json, so a value here has no effect while it is set.`;
            }
            if (!isSet(field.key)) {
                return `Not in config.json, so the environment's value applies: ${shown}.`;
            }
            return "";
        }

        function confirmationProblem(field) {
            if (!confirmKeys.has(field.key) || !isSet(field.key)) return null;
            return confirmValues.get(field.key) === draft[field.key] ? null : "The two entries do not match.";
        }

        function matchesFilter(field) {
            if (filter.onlySet && !isSet(field.key) && !isChanged(field.key)) return false;
            if (!filter.query) return true;
            const haystack = `${field.key} ${field.label} ${field.help}`.toLowerCase();
            return filter.query.split(/\s+/).every((term) => haystack.includes(term));
        }

        function refreshView(view) {
            const { field } = view;
            const setHere = isSet(field.key);
            const changedHere = isChanged(field.key)
                || (field.nicknames_key && isChanged(field.nicknames_key))
                || (field.nwr_key && isChanged(field.nwr_key));

            view.root.hidden = !visible(field) || !matchesFilter(field);
            view.root.classList.toggle("is-set", setHere);
            view.root.classList.toggle("is-changed", !!changedHere);

            const badges = [];
            if (changedHere) {
                badges.push(options.enforceRequired
                    ? badge("set", "changed", "Will be written to config.json")
                    : badge("changed", "changed", "Differs from the saved config.json"));
            }
            if (field.environment && (environmentWins(field) || !setHere)) {
                badges.push(badge("environment", "env", "Set by an environment variable"));
            } else if (!setHere) {
                badges.push(badge("default", "default", "Not in config.json"));
            }
            if (field.restart) badges.push(badge("restart", "restart", "Read at startup: a reload does not apply a change"));
            view.badges.replaceChildren(...badges);
            view.reset.hidden = !setHere;

            const note = environmentNote(field);
            view.note.textContent = note;
            view.note.hidden = !note;

            const problem = serverErrors.get(field.key)
                || (touched.has(field.key) ? localErrors.get(field.key) || confirmationProblem(field) : null);
            view.error.textContent = problem || "";
            view.error.hidden = !problem;
            view.root.classList.toggle("has-error", !!problem);
        }

        function refresh() {
            for (const view of views.values()) refreshView(view);
            for (const [groupId, section] of groupSections) {
                const anyVisible = [...views.values()].some((view) => view.field.group === groupId && !view.root.hidden);
                section.classList.toggle("is-empty", !anyVisible);
            }
        }

        // ----- public -----

        function renderGroup(groupId) {
            const group = schema.groups.find((each) => each.id === groupId);
            const list = el("div", { class: "cfg-fields" });
            for (const field of fields.filter((each) => each.group === groupId)) {
                const node = buildField(field);
                if (node) list.append(node);
            }
            const section = el("section", { class: "cfg-group", id: `cfg-group-${groupId}`, "data-group": groupId },
                el("div", { class: "cfg-group-head" },
                    el("h2", { text: group.title }),
                    el("p", { class: "cfg-group-summary", text: group.summary })),
                list);
            groupSections.set(groupId, section);
            refresh();
            return section;
        }

        /** config.json as it would be saved: the file's own key order, new keys in form order. */
        function output() {
            const out = {};
            for (const key of Object.keys(original)) {
                if (hasOwn(draft, key)) out[key] = draft[key];
            }
            for (const field of fields) {
                if (hasOwn(draft, field.key) && !hasOwn(out, field.key)) out[field.key] = draft[field.key];
            }
            for (const key of Object.keys(draft)) {
                if (!hasOwn(out, key)) out[key] = draft[key];
            }
            return out;
        }

        function changedKeys() {
            const keys = new Set([...Object.keys(original), ...Object.keys(draft)]);
            return [...keys].filter(isChanged);
        }

        function changes() {
            return changedKeys().map((key) => {
                const field = byKey.get(key);
                const secret = field && field.type === "secret";
                const show = (object) => (hasOwn(object, key) ? (secret ? "(hidden)" : describe(object[key])) : "(default)");
                return { key, label: field ? field.label : key, before: show(original), after: show(draft), restart: !!(field && field.restart) };
            });
        }

        /** Keys a server error names, so the fields can be flagged. Returns them in form order. */
        function markErrors(message) {
            serverErrors.clear();
            const keys = [];
            for (const field of fields) {
                if (new RegExp(`\\b${field.key}\\b`).test(message)) {
                    const shown = field.type === "managed"
                        ? fields.find((each) => each.nicknames_key === field.key || each.nwr_key === field.key)
                        : field;
                    if (shown) {
                        serverErrors.set(shown.key, message);
                        keys.push(shown.key);
                    }
                }
            }
            refresh();
            return keys;
        }

        /** Problems the page can find without asking the server, limited to `groupIds` if given. */
        function problems(groupIds) {
            const found = [];
            for (const field of fields) {
                if (groupIds && !groupIds.includes(field.group)) continue;
                const view = views.get(field.key);
                const shown = view && !view.root.hidden;
                if (localErrors.has(field.key) && shown) {
                    found.push({ key: field.key, message: localErrors.get(field.key) });
                    continue;
                }
                const mismatch = confirmationProblem(field);
                if (mismatch) {
                    found.push({ key: field.key, message: mismatch });
                    continue;
                }
                if (options.enforceRequired && field.setup_required && !satisfied(field)) {
                    found.push({ key: field.key, message: `The listener cannot start without this (${field.label}).` });
                }
            }
            for (const problem of found) touched.add(problem.key);
            for (const problem of found) {
                if (!localErrors.has(problem.key) && !confirmationProblem(byKey.get(problem.key))) {
                    serverErrors.set(problem.key, problem.message);
                }
            }
            refresh();
            return found;
        }

        function satisfied(field) {
            if (field.environment) return true;
            if (!isSet(field.key)) return false;
            const value = draft[field.key];
            if (typeof value === "string") return value.trim() !== "";
            if (Array.isArray(value)) return value.length > 0;
            return true;
        }

        const api = {
            renderGroup,
            output,
            text: () => `${JSON.stringify(output(), null, 4)}\n`,
            changedKeys,
            changes,
            markErrors,
            problems,
            focus(key) {
                const view = views.get(key);
                if (!view) return;
                view.root.scrollIntoView({ behavior: "smooth", block: "center" });
                const input = view.root.querySelector("input, select, textarea");
                if (input) input.focus({ preventScroll: true });
            },
            clearErrors() {
                serverErrors.clear();
                refresh();
            },
            annotate(key, text, kind) {
                const view = views.get(key);
                if (!view) return;
                view.annotation.textContent = text || "";
                view.annotation.hidden = !text;
                view.annotation.className = `cfg-annotation${kind ? ` cfg-annotation-${kind}` : ""}`;
            },
            setFilter(next) {
                filter = Object.assign({}, filter, next);
                filter.query = (filter.query || "").trim().toLowerCase();
                refresh();
            },
            /** Makes `value` the draft, as when the JSON view is applied. */
            replaceDraft(value) {
                draft = clone(value);
                serverErrors.clear();
                localErrors.clear();
                for (const view of views.values()) view.rebuild();
                changed();
            },
            /** Makes the current draft the saved state, after a successful save. */
            commit() {
                original = clone(draft);
                refresh();
            },
            /** Presets a key only when nothing, the environment included, has set it yet. */
            suggest(key, value) {
                const field = byKey.get(key);
                if (!field || isSet(key) || field.environment) return;
                draft[key] = value;
                const view = views.get(key);
                if (view) view.rebuild();
                changed();
            },
            unknownKeys() {
                return Object.keys(draft).filter((key) => !byKey.has(key));
            },
            groupOf(key) {
                const field = byKey.get(key);
                return field ? field.group : null;
            },
            countSet(groupId) {
                return fields.filter((field) => field.group === groupId && field.type !== "managed" && isSet(field.key)).length;
            },
            countChanged(groupId) {
                return fields.filter((field) => field.group === groupId && isChanged(field.key)).length;
            },
        };
        return api;
    }

    window.ConfigForm = { create, describe };
})();
