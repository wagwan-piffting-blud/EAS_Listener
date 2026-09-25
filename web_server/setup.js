/**
 * First-run setup: walks through the settings the listener cannot start without, then saves
 * config.json and waits for the listener to take over.
 *
 * Only what is filled in is written. Everything else keeps the listener's own default, exactly as
 * if the key had never been mentioned, so nothing here applies a default on the user's behalf.
 */
(function () {
    const TOKEN_KEY = "easSetupToken";
    const POLL_INTERVAL_MS = 1500;

    const body = document.getElementById("setupBody");
    const stepsList = document.getElementById("setupSteps");
    const status = document.getElementById("setupStatus");
    const actions = document.getElementById("setupActions");
    const backButton = document.getElementById("backButton");
    const nextButton = document.getElementById("nextButton");

    let token = takeToken();
    let info = null;
    let schema = null;
    let form = null;
    let steps = [];
    let current = 0;
    let preview = null;
    // What /api/setup/autostart says this machine can do, and the answer to it: "yes", "no",
    // or null while unanswered.
    let autostart = null;
    let autostartChoice = null;
    let autostartBox = null;
    let notifications = null;

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

    function show(kind, message) {
        status.className = `config-status ${kind}`;
        status.textContent = message;
        status.hidden = false;
    }

    function hideStatus() {
        status.hidden = true;
    }

    /** Takes the token out of the address bar, so it does not linger in history or screenshots. */
    function takeToken() {
        const params = new URLSearchParams(window.location.search);
        const fromUrl = (params.get("token") || "").trim();
        if (fromUrl) {
            try {
                sessionStorage.setItem(TOKEN_KEY, fromUrl);
            } catch (err) {
                // Storage can be unavailable; the token still works for this page load.
            }
            params.delete("token");
            const rest = params.toString();
            window.history.replaceState(null, "", `${window.location.pathname}${rest ? `?${rest}` : ""}`);
            return fromUrl;
        }
        try {
            return sessionStorage.getItem(TOKEN_KEY) || "";
        } catch (err) {
            return "";
        }
    }

    function rememberToken(value) {
        token = value;
        try {
            sessionStorage.setItem(TOKEN_KEY, value);
        } catch (err) {
            // As above.
        }
    }

    function setupFetch(path, options = {}) {
        const headers = Object.assign({}, options.headers, { "X-Setup-Token": token });
        return fetch(path, Object.assign({}, options, { headers, credentials: "same-origin", cache: "no-store" }));
    }

    async function fetchJson(path) {
        const response = await setupFetch(path);
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return response.json();
    }

    async function loadStatus() {
        const response = await fetch("/api/setup/status", { cache: "no-store" });
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return response.json();
    }

    /** True when the token is accepted. */
    async function loadSchema() {
        const response = await setupFetch("/api/config/schema");
        if (response.status === 401) return false;
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        schema = await response.json();
        return true;
    }

    // ----- before the steps -----

    function showTokenStep(message) {
        actions.hidden = true;
        stepsList.hidden = true;
        const input = el("input", {
            type: "text",
            class: "cfg-input cfg-mono",
            id: "setupToken",
            autocomplete: "off",
            spellcheck: "false",
            placeholder: "32 letters and digits",
            "aria-label": "Setup token",
        });
        const submit = el("button", { type: "submit", class: "custom-button cfg-primary", text: "Continue" });
        const formNode = el("form", { class: "cfg-row" }, input, submit);
        formNode.addEventListener("submit", async (event) => {
            event.preventDefault();
            const candidate = input.value.trim();
            if (!candidate) return;
            submit.disabled = true;
            rememberToken(candidate);
            try {
                if (await loadSchema()) {
                    hideStatus();
                    begin();
                } else {
                    show("bad", "That token is not the one this listener printed. Copy it again from the console output or the file named above.");
                }
            } catch (err) {
                show("bad", `Could not reach the listener: ${err.message}`);
            } finally {
                submit.disabled = false;
            }
        });

        body.replaceChildren(
            el("h2", { text: "Welcome" }),
            el("p", { class: "setup-lede" },
                "This listener has no configuration yet, so it is waiting for you to set it up. To prove this is your install, enter the setup token it printed when it started. It is also saved in ",
                el("code", { text: info.token_file }),
                "."),
            el("p", { class: "setup-lede" }, "In Docker, docker logs eas_listener shows it."),
            formNode,
        );
        if (message) show("bad", message);
        input.focus();
    }

    // ----- the steps -----

    async function begin() {
        // Only asked once the token is accepted, which both ways in reach here. Optional: without
        // an answer the start-at-boot question is simply not asked.
        autostart = await fetchJson("/api/setup/autostart").catch(() => null);

        form = window.ConfigForm.create({
            schema,
            value: {},
            fetchJson,
            confirmSecrets: ["DASHBOARD_PASSWORD"],
            enforceRequired: true,
            onChange: () => {
                if (steps[current] && steps[current].id === "review") updatePreview();
            },
        });

        const browserZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
        const timeZone = schema.fields.find((field) => field.key === "TZ");
        if (browserZone && timeZone && timeZone.options.includes(browserZone)) form.suggest("TZ", browserZone);

        const essential = schema.groups.filter((group) => group.essential);
        const optional = schema.groups.filter((group) => !group.essential);

        steps = essential.map((group, index) => {
            const node = el("div", {});
            if (index === 0) {
                node.append(el("p", { class: "setup-lede" },
                    "These steps create ",
                    el("code", { text: info.config_path }),
                    ". Anything you leave alone keeps the listener's built-in default and is not written to the file, so it can be changed later from the dashboard's configuration page."));
                if (info.config_json_is_directory) {
                    node.append(el("p", { class: "setup-lede" },
                        "config.json is a folder here, which is what Docker makes when the file it mounts does not exist yet. That works: the configuration is kept inside it, and the dashboard edits it there."));
                }
            }
            node.append(form.renderGroup(group.id));
            return { id: group.id, title: group.title, groups: [group.id], node };
        });

        notifications = window.NotificationEditor.create({ request: setupFetch, basePath: "/api/setup/notifications" });
        notifications.load().catch((err) => {
            notifications.node.append(el("p", { class: "cfg-note", text: `The notification list could not be loaded: ${err.message}` }));
        });
        steps.push({
            id: "notifications",
            title: "Notifications",
            groups: [],
            node: el("div", {},
                el("p", { class: "setup-lede" },
                    "Optional: where each alert should be announced -- Discord, Telegram, email, ntfy, Pushover and more than a hundred others. Add as many as you like and send each one a test. This can also be done later from the dashboard's configuration page."),
                notifications.node),
        });

        preview = el("pre", { class: "setup-preview", "aria-label": "config.json to be saved" });
        const optionalBox = el("details", { class: "setup-optional" },
            el("summary", { text: "Everything else (optional)" }),
            optional.map((group) => form.renderGroup(group.id)));
        steps.push({
            id: "review",
            title: "Review",
            groups: optional.map((group) => group.id),
            node: el("div", {},
                el("h2", { text: "Review and start" }),
                autostartSection(),
                el("p", { class: "setup-lede" }, "The rest is optional. This is exactly what will be saved:"),
                preview,
                optionalBox),
        });

        stepsList.replaceChildren(...steps.map((step, index) => el("li", {
            "data-index": index,
            text: step.title,
            onclick: () => {
                if (index < current) go(index);
            },
        })));
        stepsList.hidden = false;
        actions.hidden = false;
        go(0);
    }

    function updatePreview() {
        if (preview) preview.textContent = form.text();
    }

    /**
     * The start-at-boot question, when the machine can act on it. Asked rather than defaulted:
     * a service outlives this page, so neither answer is picked for the user.
     */
    function autostartSection() {
        if (!autostart || autostart.mode === "unavailable") return null;
        const windows = autostart.kind === "windows_service";

        if (autostart.mode === "manual") {
            return el("fieldset", { class: "setup-autostart" },
                el("legend", { text: "Start with this computer" }),
                el("p", { class: "setup-lede" }, windows
                    ? "To have it start with Windows, stop this listener once setup is done, then run this from an administrator terminal:"
                    : "To have it start at boot, stop this listener once setup is done (Ctrl+C in its terminal), then run:"),
                el("code", { text: autostart.command }));
        }

        const what = {
            windows_service: "Installs EAS Listener as a Windows service: it starts with Windows, before anyone signs in, and restarts itself if it stops.",
            systemd: "Installs a systemd unit, eas-listener.service: it starts at boot and restarts itself if it stops.",
            launchd_daemon: "Installs a LaunchDaemon: it starts when this Mac boots, before anyone logs in, and restarts itself if it stops.",
            launchd_agent: "Installs a LaunchAgent for your account: it starts whenever you log in to this Mac, with its menu bar icon, and restarts itself if it stops. To start at boot instead, before anyone logs in, run it with sudo and --install-service later.",
        }[autostart.kind] || "Starts it automatically from now on.";
        const prompt = autostart.prompt ? " Windows will ask for administrator permission when you save." : "";
        const choice = (value, label) => el("label", { class: "setup-autostart-choice" },
            el("input", {
                type: "radio",
                name: "setupAutostart",
                value,
                checked: autostartChoice === value,
                onchange: () => {
                    autostartChoice = value;
                    autostartBox.classList.remove("is-missing");
                    hideStatus();
                },
            }),
            el("span", { text: label }));

        autostartBox = el("fieldset", { class: "setup-autostart" },
            el("legend", { text: "Start with this computer?" }),
            el("p", { class: "setup-lede", text: what + prompt }),
            choice("yes", "Yes, start it automatically"),
            choice("no", "No, only when I start it myself"));
        return autostartBox;
    }

    function go(index) {
        current = index;
        body.replaceChildren(steps[index].node);
        [...stepsList.children].forEach((item, itemIndex) => {
            item.classList.toggle("is-current", itemIndex === index);
            item.classList.toggle("is-done", itemIndex < index);
            item.style.cursor = itemIndex < index ? "pointer" : "";
        });
        backButton.hidden = index === 0;
        nextButton.textContent = index === steps.length - 1 ? "Save and start the listener" : "Next";
        if (steps[index].id === "review") updatePreview();
        document.querySelector("main").scrollTo({ top: 0 });
        window.scrollTo({ top: 0 });
    }

    function stepFor(key) {
        const group = form.groupOf(key);
        const index = steps.findIndex((step) => step.groups.includes(group));
        return index === -1 ? steps.length - 1 : index;
    }

    backButton.addEventListener("click", () => {
        hideStatus();
        if (current > 0) go(current - 1);
    });

    nextButton.addEventListener("click", () => {
        if (current < steps.length - 1) {
            const problems = form.problems(steps[current].groups);
            if (problems.length > 0) {
                show("bad", problems.length === 1 ? problems[0].message : "A few settings on this page still need attention.");
                form.focus(problems[0].key);
                return;
            }
            // Caught here rather than at the end: the dashboard refuses the shipped defaults.
            const chosen = form.output();
            const refused = [
                chosen.DASHBOARD_USERNAME === "admin" ? "DASHBOARD_USERNAME cannot be admin" : null,
                chosen.DASHBOARD_PASSWORD === "password" ? "DASHBOARD_PASSWORD cannot be password" : null,
            ].filter(Boolean);
            if (steps[current].groups.includes("dashboard") && refused.length > 0) {
                const message = `${refused.join(", and ")}: the dashboard refuses the shipped defaults, so nobody could sign in.`;
                form.focus(form.markErrors(message)[0]);
                show("bad", message);
                return;
            }
            hideStatus();
            go(current + 1);
            return;
        }
        finish();
    });

    async function finish() {
        const problems = form.problems();
        if (problems.length > 0) {
            go(stepFor(problems[0].key));
            show("bad", problems[0].message);
            form.focus(problems[0].key);
            return;
        }
        if (autostartBox && autostartChoice === null) {
            autostartBox.classList.add("is-missing");
            autostartBox.scrollIntoView({ block: "center" });
            show("bad", "Choose whether it should start with this computer.");
            return;
        }

        const installService = autostartBox !== null && autostartChoice === "yes";
        nextButton.disabled = true;
        backButton.disabled = true;
        show("busy", installService && autostart.prompt
            ? "Saving. Windows is asking for administrator permission to install the service; answer its prompt to continue."
            : installService ? "Saving and installing the service..." : "Checking and saving...");
        try {
            // First, because saving config.json ends setup.
            if (notifications && notifications.dirty()) {
                const saved = await notifications.save({ path: form.output().APPRISE_CONFIG_PATH || null });
                if (!saved.ok) {
                    go(steps.findIndex((step) => step.id === "notifications"));
                    show("bad", `The notification list was not saved, so nothing was:\n${saved.error}`);
                    return;
                }
            }
            const response = await setupFetch(`/api/setup/config${installService ? "?service=install" : ""}`, {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: form.text(),
            });
            if (response.status === 401) {
                showTokenStep("The setup token was not accepted. Enter it again.");
                return;
            }
            const result = await response.json().catch(() => null);
            if (!response.ok || !result || !result.ok) {
                const error = result && result.error ? result.error : `HTTP ${response.status}`;
                const keys = form.markErrors(error);
                if (keys.length > 0) {
                    go(stepFor(keys[0]));
                    form.focus(keys[0]);
                }
                show("bad", `Not saved:\n${error}`);
                return;
            }
            try {
                sessionStorage.removeItem(TOKEN_KEY);
            } catch (err) {
                // Nothing to clean up.
            }
            waitForListener(result);
        } catch (err) {
            show("bad", `Could not reach the listener: ${err.message}`);
        } finally {
            nextButton.disabled = false;
            backButton.disabled = false;
        }
    }

    // ----- handing over -----

    function waitForListener(result) {
        actions.hidden = true;
        stepsList.hidden = true;
        hideStatus();

        const defaultPort = window.location.protocol === "https:" ? "443" : "80";
        const samePort = String(result.port) === (window.location.port || defaultPort);
        const origin = samePort ? "" : `${window.location.protocol}//${window.location.hostname}:${result.port}`;
        const target = `${origin}/login.html`;

        const service = result.service || null;
        const message = el("p", {}, result.restarting
            ? "Saved. The container is restarting its startup sequence to apply it. That takes a few seconds, or a few minutes if a Cepstral voice has to be downloaded first."
            : service && service.installed
                ? "Saved, and installed as a service. It is starting now, and will start by itself from now on."
                : "Saved. The listener is starting.");
        // config.json is saved either way, so a service that did not install is a note, not a stop.
        const serviceProblem = service && !service.installed
            ? el("p", { class: "setup-lede" },
                `It could not be set up to start with this computer: ${service.error} It is starting normally instead; `,
                el("code", { text: "--install-service" }),
                " sets that up later.")
            : null;
        // replaceChildren would print a null as the text "null".
        body.replaceChildren(...[
            el("h2", { text: "Starting the listener" }),
            el("div", { class: "setup-waiting" }, el("span", { class: "setup-spinner" }), message),
            serviceProblem,
            samePort ? null : el("p", { class: "setup-lede" }, "The dashboard now listens on port ", el("code", { text: String(result.port) }), "."),
            el("p", { class: "setup-lede" }, "You will be taken to the sign-in page when it is ready. If ffmpeg is not installed yet, the listener downloads it first, which can take a few minutes."),
        ].filter(Boolean));

        // Long enough for a first start that downloads ffmpeg, or a Cepstral voice in Docker.
        const deadline = Date.now() + 10 * 60 * 1000;

        async function listenerIsUp() {
            if (!samePort) {
                // Another origin cannot be read, but a no-cors request still fails while nothing listens.
                await fetch(`${origin}/api/health`, { mode: "no-cors", cache: "no-store" });
                return true;
            }
            const state = await loadStatus();
            return state.phase === "running";
        }

        async function poll() {
            try {
                if (await listenerIsUp()) {
                    window.location.href = target;
                    return;
                }
            } catch (err) {
                // Nothing is listening during the handover; keep waiting.
            }
            if (Date.now() > deadline) {
                body.append(el("p", {},
                    "The listener has not come back yet. config.json is saved, so check its console output (docker logs eas_listener in Docker) for what stopped it, then ",
                    el("a", { href: target, text: "open the sign-in page" }),
                    "."));
                return;
            }
            setTimeout(poll, POLL_INTERVAL_MS);
        }
        setTimeout(poll, POLL_INTERVAL_MS);
    }

    // ----- start -----

    async function start() {
        try {
            info = await loadStatus();
        } catch (err) {
            body.replaceChildren(el("p", { class: "setup-lede", text: `Could not reach the listener: ${err.message}` }));
            return;
        }

        if (info.phase === "running") {
            window.location.href = "/index.html";
            return;
        }
        if (!info.setup_required) {
            waitForListener({ port: window.location.port || (window.location.protocol === "https:" ? 443 : 80), restarting: false });
            return;
        }
        try {
            if (!token || !(await loadSchema())) {
                showTokenStep(token ? "That setup token was not accepted. Enter the one this listener printed." : null);
                return;
            }
        } catch (err) {
            body.replaceChildren(el("p", { class: "setup-lede", text: `Could not load the settings: ${err.message}` }));
            return;
        }
        begin();
    }

    start();
})();
