/**
 * The configuration editor: config.json as a form built from /api/config/schema, with the raw
 * JSON one click away.
 *
 * Validation is deliberately server-side: /api/config?dry_run=true loads the candidate exactly the
 * way startup does, so the rules here can never drift from the rules that actually matter.
 */
(function () {
    const formHost = document.getElementById("cfgForm");
    const editor = document.getElementById("configEditor");
    const status = document.getElementById("configStatus");
    const intro = document.getElementById("cfgIntro");
    const nav = document.getElementById("cfgNav");
    const dirtyLabel = document.getElementById("cfgDirty");
    const changesBox = document.getElementById("cfgChanges");
    const changeList = document.getElementById("cfgChangeList");
    const jsonView = document.getElementById("cfgJson");
    const viewFormButton = document.getElementById("viewForm");
    const viewJsonButton = document.getElementById("viewJson");
    const search = document.getElementById("cfgSearch");
    const onlySet = document.getElementById("cfgOnlySet");
    if (!formHost || !editor) return;

    const BUTTONS = ["validateButton", "saveButton", "saveReloadButton", "revertButton"];

    let schema = null;
    let form = null;
    let savedText = "";
    let view = "form";
    let jsonEdited = false;
    let unknownSection = null;
    const navLinks = new Map();
    // apprise.yml, edited alongside config.json and saved with it.
    const notifications = window.NotificationEditor.create({
        request: window.apiFetch,
        basePath: "/api/notifications",
        onChange: () => updateDirty(),
    });
    let notificationsLoaded = false;

    function show(kind, message) {
        status.className = `config-status ${kind}`;
        status.textContent = message;
        status.hidden = false;
    }

    function setBusy(busy) {
        for (const id of BUTTONS) {
            const button = document.getElementById(id);
            if (button) button.disabled = busy;
        }
    }

    function signedOut(response) {
        if (response.status !== 401) return false;
        window.location.href = "/login.html?redirect=/config.html";
        return true;
    }

    function parseEditor() {
        try {
            const value = JSON.parse(editor.value);
            if (!value || typeof value !== "object" || Array.isArray(value)) {
                return { error: "config.json has to be a JSON object: { ... }" };
            }
            return { value };
        } catch (err) {
            return { error: err.message };
        }
    }

    async function fetchJson(path) {
        const response = await window.apiFetch(path);
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return response.json();
    }

    // ----- building the page -----

    function renderIntro() {
        const environmentKeys = schema.fields.filter((field) => field.environment).map((field) => field.key);
        const parts = [
            "Editing ",
            Object.assign(document.createElement("code"), { textContent: schema.config_path }),
            ". Changes are checked with the same rules the listener applies at startup, and the previous file is kept as config.json.bak. A setting left at its default is not written to the file.",
        ];
        if (environmentKeys.length > 0) {
            parts.push(schema.precedence === "file"
                ? ` ${environmentKeys.length} settings also have an environment value, which applies only where config.json is silent.`
                : ` ${environmentKeys.length} settings are set by environment variables, which override config.json.`);
        }
        intro.replaceChildren(...parts);
    }

    function renderNav() {
        nav.replaceChildren();
        navLinks.clear();
        for (const group of schema.groups) {
            const count = document.createElement("span");
            count.className = "cfg-nav-count";
            const link = document.createElement("a");
            link.href = `#cfg-group-${group.id}`;
            link.append(group.title, count);
            nav.append(link);
            navLinks.set(group.id, { link, count });
            if (group.id === "relay") appendNotificationsLink();
        }
        if (!navLinks.has("notifications")) appendNotificationsLink();

        markCurrentSection();
    }

    function appendNotificationsLink() {
        const count = document.createElement("span");
        count.className = "cfg-nav-count";
        const link = document.createElement("a");
        link.href = "#cfg-group-notifications";
        link.append("Notifications", count);
        nav.append(link);
        navLinks.set("notifications", { link, count });
    }

    /** The current section is the last one whose top has scrolled past the upper quarter. */
    function markCurrentSection() {
        const line = window.innerHeight * 0.25;
        let currentId = null;
        for (const section of formHost.querySelectorAll(".cfg-group[data-group]")) {
            if (section.getBoundingClientRect().top <= line || currentId === null) currentId = section.dataset.group;
        }
        for (const [groupId, { link }] of navLinks) link.classList.toggle("is-current", groupId === currentId);
    }

    // The page scrolls inside <main> on wide screens and the window on narrow ones.
    document.getElementById("configPage").addEventListener("scroll", markCurrentSection, { passive: true });
    window.addEventListener("scroll", markCurrentSection, { passive: true });

    function renderUnknown() {
        const keys = form.unknownKeys();
        if (unknownSection) unknownSection.remove();
        unknownSection = null;
        if (keys.length === 0) return;

        const list = document.createElement("ul");
        list.className = "cfg-unknown-list";
        for (const key of keys) {
            const item = document.createElement("li");
            item.append(Object.assign(document.createElement("code"), { textContent: key }));
            list.append(item);
        }
        const heading = document.createElement("h2");
        heading.textContent = "Other keys";
        const note = document.createElement("p");
        note.className = "cfg-group-summary";
        note.textContent = "config.json also holds these, which the listener does not read. They are kept exactly as they are; edit them in the JSON view.";
        const head = document.createElement("div");
        head.className = "cfg-group-head";
        head.append(heading, note);

        unknownSection = document.createElement("section");
        unknownSection.className = "cfg-group";
        unknownSection.append(head, list);
        formHost.append(unknownSection);
    }

    async function annotateComponents() {
        try {
            const payload = await fetchJson("/api/components");
            for (const component of payload.components || []) {
                if (component.present) {
                    const version = component.version ? ` (${component.version})` : "";
                    form.annotate(component.config_key, `In use: ${component.path}${version}`, "ok");
                } else {
                    form.annotate(
                        component.config_key,
                        `Not found: ${component.path}`,
                        component.requirement === "required" ? "bad" : null,
                    );
                }
            }
        } catch (err) {
            console.warn("Component status is unavailable:", err);
        }
    }

    function buildForm(value) {
        form = window.ConfigForm.create({ schema, value, fetchJson, onChange: formChanged });
        formHost.replaceChildren(...schema.groups.map((group) => form.renderGroup(group.id)));
        const relay = formHost.querySelector("#cfg-group-relay");
        if (relay) relay.after(notifications.node);
        else formHost.append(notifications.node);
        if (!notificationsLoaded) {
            notificationsLoaded = true;
            notifications.load().catch((err) => show("bad", `Could not load the notification list: ${err.message}`));
        }
        renderUnknown();
        renderNav();
        applyFilter();
        annotateComponents();
        editor.value = form.text();
        updateDirty();
    }

    function formChanged() {
        if (view === "form") editor.value = form.text();
        renderUnknown();
        updateDirty();
    }

    function updateDirty() {
        const changes = form ? form.changes() : [];
        const notificationChange = notifications.change();
        if (notificationChange) changes.push(notificationChange);
        let label;
        if (view === "json" && jsonEdited) label = "Unsaved edits in the JSON view";
        else if (changes.length === 0) label = "No unsaved changes";
        else label = `${changes.length} unsaved ${changes.length === 1 ? "change" : "changes"}`;
        dirtyLabel.textContent = label;
        dirtyLabel.classList.toggle("has-changes", label !== "No unsaved changes");

        changeList.replaceChildren(...changes.map((change) => {
            const item = document.createElement("li");
            const key = Object.assign(document.createElement("code"), { textContent: change.key });
            const values = document.createElement("span");
            values.className = "cfg-change-values";
            values.textContent = ` ${change.before} → ${change.after}${change.restart ? " (applies after a restart)" : ""}`;
            item.append(key, values);
            return item;
        }));
        changesBox.hidden = changes.length === 0 || view === "json";

        for (const [groupId, { count }] of navLinks) {
            const changed = groupId === "notifications"
                ? (notificationChange ? 1 : 0)
                : form ? form.countChanged(groupId) : 0;
            count.textContent = changed ? String(changed) : "";
        }
    }

    function hasUnsavedWork() {
        return (form && form.changedKeys().length > 0) || jsonEdited || notifications.dirty();
    }

    // ----- views -----

    function setView(next) {
        if (next === view) return;
        if (next === "form") {
            const parsed = parseEditor();
            if (parsed.error) {
                show("bad", `Fix the JSON before going back to the form:\n${parsed.error}`);
                return;
            }
            if (!form) buildForm(parsed.value);
            else if (jsonEdited) form.replaceDraft(parsed.value);
            jsonEdited = false;
        } else if (form) {
            editor.value = form.text();
            jsonEdited = false;
        }

        view = next;
        formHost.hidden = next !== "form";
        jsonView.hidden = next !== "json";
        viewFormButton.setAttribute("aria-pressed", String(next === "form"));
        viewJsonButton.setAttribute("aria-pressed", String(next === "json"));
        search.disabled = next !== "form";
        onlySet.disabled = next !== "form";
        nav.hidden = next !== "form";
        updateDirty();
    }

    viewFormButton.addEventListener("click", () => setView("form"));
    viewJsonButton.addEventListener("click", () => setView("json"));
    editor.addEventListener("input", () => {
        jsonEdited = true;
        updateDirty();
    });
    function applyFilter() {
        if (form) form.setFilter({ query: search.value, onlySet: onlySet.checked });
        notifications.setFilter({ query: search.value, onlySet: onlySet.checked });
    }
    search.addEventListener("input", applyFilter);
    onlySet.addEventListener("change", applyFilter);

    // ----- loading and saving -----

    async function load() {
        try {
            const [configResponse, schemaResponse] = await Promise.all([
                window.apiFetch("/api/config"),
                window.apiFetch("/api/config/schema"),
            ]);
            if (signedOut(configResponse) || signedOut(schemaResponse)) return;
            if (!configResponse.ok) throw new Error(`config.json: HTTP ${configResponse.status}`);
            if (!schemaResponse.ok) throw new Error(`schema: HTTP ${schemaResponse.status}`);

            savedText = await configResponse.text();
            schema = await schemaResponse.json();
        } catch (err) {
            show("bad", `Could not load the configuration: ${err.message}`);
            return;
        }

        renderIntro();
        editor.value = savedText;
        const parsed = parseEditor();
        if (parsed.error) {
            // The form needs an object to edit; the JSON view can still repair the file.
            setView("json");
            show("bad", `config.json is not valid JSON, so only the JSON view is available until it is fixed:\n${parsed.error}`);
            return;
        }
        buildForm(parsed.value);
    }

    /** The body to send, or an error explaining why there is none. */
    function candidate() {
        if (view === "json" || !form) {
            const parsed = parseEditor();
            if (parsed.error) return { error: `That is not valid JSON:\n${parsed.error}` };
            return { text: editor.value, value: parsed.value };
        }
        const problems = form.problems();
        if (problems.length > 0) {
            form.focus(problems[0].key);
            return { error: `Fix the highlighted ${problems.length === 1 ? "setting" : "settings"} first.` };
        }
        return { text: form.text(), value: form.output() };
    }

    async function submit(body, dryRun) {
        setBusy(true);
        show("busy", dryRun ? "Validating..." : "Saving...");
        if (form) form.clearErrors();
        try {
            const response = await window.apiFetch(`/api/config${dryRun ? "?dry_run=true" : ""}`, {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body,
            });
            if (signedOut(response)) return null;

            const result = await response.json().catch(() => null);
            if (!response.ok || !result || !result.ok) {
                const error = result && result.error ? result.error : `HTTP ${response.status}`;
                show("bad", `Rejected:\n${error}`);
                if (form && view === "form") {
                    const keys = form.markErrors(error);
                    if (keys.length > 0) form.focus(keys[0]);
                }
                return null;
            }
            return result;
        } catch (err) {
            show("bad", `Request failed: ${err.message}`);
            return null;
        } finally {
            setBusy(false);
        }
    }

    document.getElementById("validateButton").addEventListener("click", async () => {
        const body = candidate();
        if (body.error) {
            show("bad", body.error);
            return;
        }
        const result = await submit(body.text, true);
        if (!result) return;
        show("ok", result.credentials_changed
            ? "Valid. Note: the sign-in details differ from the running ones, so saving and reloading will sign you out."
            : "Valid. Nothing has been written.");
    });

    async function save(thenReload) {
        const body = candidate();
        if (body.error) {
            show("bad", body.error);
            return;
        }
        const restartKeys = form ? form.changes().filter((change) => change.restart).map((change) => change.key) : [];

        const result = await submit(body.text, false);
        if (!result) return;

        let notificationLine = null;
        if (notifications.dirty()) {
            const saved = await notifications.save();
            if (!saved.ok) {
                show("bad", `config.json was saved, but the notification list was not:\n${saved.error}`);
                updateDirty();
                return;
            }
            notificationLine = `The notification list (${saved.count} ${saved.count === 1 ? "URL" : "URLs"}) is saved to ${saved.path}; it applies from the next alert.`;
        }

        savedText = body.text;
        jsonEdited = false;
        if (!form) buildForm(body.value);
        else {
            if (view === "json") form.replaceDraft(body.value);
            form.commit();
        }
        updateDirty();

        const lines = ["Saved."];
        if (result.backup_path) lines.push(`The previous configuration is in ${result.backup_path}`);
        if (notificationLine) lines.push(notificationLine);

        if (thenReload) {
            try {
                const response = await window.apiFetch("/api/reload", { method: "POST" });
                lines.push(response.ok
                    ? "Reload requested; the listener is applying it now."
                    : `The reload request failed (HTTP ${response.status}). Reload from the dashboard.`);
            } catch (err) {
                lines.push(`The reload request failed: ${err.message}`);
            }
        } else {
            lines.push("Nothing is applied until the configuration is reloaded: use Save & Reload, or reload from the dashboard.");
        }

        if (restartKeys.length > 0) {
            lines.push(`These are only read at startup, so they apply after a restart: ${restartKeys.join(", ")}.`);
        }

        if (result.credentials_changed) {
            if (thenReload) {
                lines.push("", "The sign-in details changed, so this session has ended. Signing out...");
                show("ok", lines.join("\n"));
                setTimeout(() => {
                    window.location.href = "/login.html";
                }, 4000);
                return;
            }
            lines.push("The new sign-in details take effect when the configuration is reloaded.");
        }

        show(restartKeys.length > 0 ? "warn" : "ok", lines.join("\n"));
    }

    document.getElementById("saveButton").addEventListener("click", () => save(false));
    document.getElementById("saveReloadButton").addEventListener("click", () => save(true));

    document.getElementById("revertButton").addEventListener("click", () => {
        editor.value = savedText;
        jsonEdited = false;
        const parsed = parseEditor();
        if (form && !parsed.error) form.replaceDraft(parsed.value);
        if (form) form.commit();
        notifications.revert();
        updateDirty();
        show("ok", "Back to the saved configuration.");
    });

    const logoutLink = document.getElementById("logoutLink");
    if (logoutLink) {
        logoutLink.addEventListener("click", async (event) => {
            event.preventDefault();
            try {
                await fetch("/api/logout", { method: "POST", credentials: "same-origin" });
            } catch (err) {
                console.warn("Logout request failed:", err);
            }
            window.location.href = "/login.html";
        });
    }

    // A half-finished edit is easy to lose by clicking away.
    window.addEventListener("beforeunload", (event) => {
        if (hasUnsavedWork()) event.preventDefault();
    });

    load();
})();
