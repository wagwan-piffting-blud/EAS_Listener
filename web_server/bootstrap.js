/**
 * Populates the globals the dashboard scripts read at module scope, then loads them in order.
 *
 * These values used to be interpolated into the page by PHP. They now come from
 * /api/dashboard-config, which means the page itself is a static file.
 */
(function () {
    function loadScript(src) {
        return new Promise((resolve, reject) => {
            const element = document.createElement("script");
            element.src = src;
            element.onload = () => resolve();
            element.onerror = () => reject(new Error(`Failed to load ${src}`));
            document.body.appendChild(element);
        });
    }

    function renderNotices(config) {
        const container = document.getElementById("notices");
        if (!container) return;

        const migrationUrl =
            "https://github.com/wagwan-piffting-blud/EAS_Listener#the--lite-image-is-deprecated";
        const blocks = [];

        if (config.deprecation_notice) {
            blocks.push(`
                <div class="notice notice-deprecation" role="alert">
                    <span class="notice-badge">Deprecated image</span>
                    <span class="notice-body">${escapeHtml(config.deprecation_notice)}
                        <a href="${migrationUrl}" target="_blank" rel="noopener">Read the migration guide.</a>
                    </span>
                </div>`);
        }

        if (config.tts_engine_fallback_reason) {
            blocks.push(`
                <div class="notice notice-tts" role="alert">
                    <span class="notice-badge">TTS fallback</span>
                    <span class="notice-body">${escapeHtml(config.tts_engine_fallback_reason)}</span>
                </div>`);
        }

        container.innerHTML = blocks.join("");
    }

    function escapeHtml(value) {
        const div = document.createElement("div");
        div.textContent = value;
        return div.innerHTML;
    }

    async function start() {
        let config;
        try {
            const response = await fetch("/api/dashboard-config", {
                credentials: "same-origin",
                headers: { Accept: "application/json" },
            });
            if (response.status === 401 || response.status === 403) {
                window.location.href = "/login.html";
                return;
            }
            if (!response.ok) throw new Error(`HTTP ${response.status}`);
            config = await response.json();
        } catch (err) {
            console.error("Could not load the dashboard configuration:", err);
            document.body.insertAdjacentHTML(
                "afterbegin",
                '<div class="notice" role="alert"><span class="notice-body">Could not reach the' +
                    " backend. The dashboard will not update until it is back.</span></div>"
            );
            return;
        }

        // Everything is served from one origin now, so there is no separate API host to point at
        // and no token to carry: the session cookie authenticates every request.
        window.API_BASE = window.location.host;
        window.APP_VERSION = config.version;
        window.MONITORING_MAX_LOGS = config.monitoring_max_logs;
        window.ALERTSOUNDENABLED = config.alert_sound_enabled === true;
        window.ALERTSOUNDDATA = config.alert_sound_enabled ? `/${config.alert_sound_src}` : "";
        window.ICECAST_STREAM_URL_MAPPING = config.icecast_stream_url_mapping || {};
        window.WATCHED_FIPS = config.watched_fips || [];
        window.TZ = config.timezone;

        renderNotices(config);

        const logoutButton = document.getElementById("logoutButton");
        if (logoutButton) {
            logoutButton.addEventListener("click", async () => {
                try {
                    await fetch("/api/logout", { method: "POST", credentials: "same-origin" });
                } catch (err) {
                    console.warn("Logout request failed:", err);
                }
                window.location.href = "/login.html";
            });
        }

        try {
            await loadScript("dashboard-init.js");
            await loadScript("index.js");
        } catch (err) {
            console.error(err);
        }
    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", start);
    } else {
        start();
    }
})();
