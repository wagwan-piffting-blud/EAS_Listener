/**
 * The configuration page's Uninstall section: what removing this instance takes with it, typed
 * confirmation, and handing the work to the listener, which does it in a process of its own and
 * stops.
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

    function create(options) {
        const request = options.request;
        const body = el("div", { class: "unin-body" }, el("p", { class: "cfg-muted", text: "Checking what an uninstall would remove..." }));
        const node = el("section", { class: "cfg-group unin", id: "cfg-group-uninstall", "data-group": "uninstall" },
            el("div", { class: "cfg-group-head" },
                el("h2", { text: "Uninstall" }),
                el("p", { class: "cfg-group-summary", text: "Removes this listener from the computer. It cannot be undone." })),
            body);

        function render(plan) {
            const items = [
                el("li", {}, "its configuration and notification list, in ", el("code", { text: plan.folder })),
            ];
            if (plan.service) items.unshift(el("li", {}, "its service, ", el("code", { text: plan.service }), ", which stops it"));
            const parts = [
                el("p", {}, "Uninstalling the instance ", el("strong", { text: plan.instance }), " removes:"),
                el("ul", { class: "unin-list" }, items),
                el("p", { class: "cfg-help" }, "Its alert archive and recordings, in ", el("code", { text: plan.data_dir }),
                    ", are kept, so nothing it recorded needs backing up first. Setting up an instance named ",
                    el("code", { text: plan.instance }), " again picks them back up."),
            ];

            if (!plan.allowed) {
                parts.push(el("p", { class: "cfg-note", text: plan.reason || "It cannot be uninstalled from here." }));
                if (plan.command) parts.push(el("p", {}, "Run this on the computer instead:"), el("pre", { class: "unin-command" }, el("code", { text: plan.command })));
                body.replaceChildren(...parts);
                return;
            }

            const deleteData = el("input", { type: "checkbox", id: "uninData" });
            const data = el("label", { class: "cfg-toggle unin-option", for: "uninData" }, deleteData,
                el("span", {}, "Also delete the alert archive and recordings"));
            // Not offered when the archive is somewhere of its own that uninstalling never touches.
            data.hidden = Boolean(plan.kept_state_dir);
            const program = el("input", { type: "checkbox", id: "uninProgram" });
            const others = plan.other_instances || [];
            const removeProgram = el("label", { class: "cfg-toggle unin-option", for: "uninProgram" }, program,
                el("span", {}, "Also remove EAS Listener itself -- the program in ", el("code", { text: plan.program_dir }),
                    " and the tools and voices it fetched",
                    others.length
                        ? el("span", { class: "cfg-muted", text: ` (kept anyway while ${others.join(", ")} still ${others.length === 1 ? "uses" : "use"} it)` })
                        : null));
            removeProgram.hidden = !plan.program_removable;
            parts.push(data, removeProgram);
            if (plan.prompts) parts.push(el("p", { class: "cfg-help", text: "Windows will ask for administrator permission on this computer to remove the service." }));

            const confirm = el("input", {
                type: "text",
                class: "cfg-input",
                autocomplete: "off",
                spellcheck: "false",
                placeholder: plan.instance,
                "aria-label": `Type ${plan.instance} to confirm`,
            });
            const button = el("button", { type: "button", class: "cfg-mini unin-button", text: "Uninstall", disabled: true });
            const result = el("p", { class: "ntf-result", hidden: true });
            confirm.addEventListener("input", () => { button.disabled = confirm.value.trim() !== plan.instance; });
            button.addEventListener("click", async () => {
                button.disabled = true;
                confirm.disabled = true;
                result.hidden = false;
                result.className = "ntf-result";
                result.textContent = "Starting the uninstall...";
                try {
                    const response = await request("/api/uninstall", {
                        method: "POST",
                        headers: { "Content-Type": "application/json" },
                        body: JSON.stringify({ confirm: confirm.value.trim(), delete_data: deleteData.checked, with_program: program.checked }),
                    });
                    const outcome = await response.json().catch(() => null);
                    if (!response.ok || !outcome || !outcome.ok) throw new Error(outcome && outcome.error ? outcome.error : `HTTP ${response.status}`);
                    body.replaceChildren(
                        el("p", { class: "ntf-result is-ok", text: "Uninstalling. The listener stops in a moment and this page will stop answering; there is nothing more to do here." }),
                        el("p", { class: "cfg-help" }, "What was removed is written to ", el("code", { text: outcome.log }), " on the computer."));
                    document.querySelectorAll(".cfg-actionbar button").forEach((other) => { other.disabled = true; });
                } catch (err) {
                    result.className = "ntf-result is-bad";
                    result.textContent = `Not uninstalled: ${err.message}`;
                    confirm.disabled = false;
                    button.disabled = confirm.value.trim() !== plan.instance;
                }
            });
            parts.push(el("div", { class: "unin-confirm" },
                el("label", { class: "unin-label" }, "Type ", el("code", { text: plan.instance }), " to confirm"),
                confirm, button), result);
            body.replaceChildren(...parts);
        }

        async function load() {
            try {
                const response = await request("/api/uninstall");
                const plan = await response.json().catch(() => null);
                if (!response.ok || !plan) throw new Error(`HTTP ${response.status}`);
                render(plan);
            } catch (err) {
                body.replaceChildren(el("p", { class: "cfg-note", text: `Could not check what an uninstall would remove: ${err.message}` }));
            }
        }

        function setFilter(filter) {
            const query = (filter.query || "").trim().toLowerCase();
            node.hidden = Boolean((query && !"uninstall remove delete".includes(query)) || filter.onlySet);
        }

        return { node, load, setFilter };
    }

    window.UninstallPanel = { create };
})();
