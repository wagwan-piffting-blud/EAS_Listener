(function () {
    const root = document.getElementById("chargen");
    const frame = document.getElementById("cgFrame");
    const viewport = document.getElementById("cgViewport");
    const track = document.getElementById("cgTrack");
    const textPrimary = document.getElementById("cgTextPrimary");
    const textClone = document.getElementById("cgTextClone");
    const audio = document.getElementById("cgAudio");
    const controls = {
        panel: document.getElementById("cgControls"),
        toggle: document.getElementById("cgControlsToggle"),
        form: document.getElementById("cgControlsForm"),
        bgColor: document.getElementById("cgBgColor"),
        textColor: document.getElementById("cgTextColor"),
        accentColor: document.getElementById("cgAccentColor"),
        fontFamily: document.getElementById("cgFontFamily"),
        fontSize: document.getElementById("cgFontSize"),
        fontWeight: document.getElementById("cgFontWeight"),
        speed: document.getElementById("cgSpeed"),
        gap: document.getElementById("cgGap"),
        textShadow: document.getElementById("cgTextShadow"),
        uppercase: document.getElementById("cgUppercase"),
        reset: document.getElementById("cgResetStyle"),
    };

    if (!root || !frame || !viewport || !track || !textPrimary || !textClone || !audio) return;

    const STYLE_STORAGE_KEY = "chargen_style_settings_v1";
    const UI_STORAGE_KEY = "chargen_ui_settings_v1";
    const DEFAULT_STYLE = {
        bgColor: "#000000",
        textColor: "#f8f8f8",
        accentColor: "#c8102e",
        fontFamily: "Courier New, Courier, monospace",
        fontSize: 56,
        fontWeight: 700,
        speed: 18,
        gap: 4,
        textShadow: "0 0 12px rgba(0, 0, 0, 0.7)",
        uppercase: true,
    };

    const defaultText = (root.dataset.defaultText || "EAS DETAILS CHANNEL").trim() || "EAS DETAILS CHANNEL";
    const token = (root.dataset.token || "").trim();
    const apiBase = (root.dataset.apiBase || window.location.host).trim();

    const state = {
        activeAlertKey: "",
        activeAlertText: "",
        currentAudioSrc: "",
        reconnectDelayMs: 2000,
        reconnectTimer: null,
        audioRetryTimer: null,
        audioAttemptVersion: 0,
        audioPendingAlertKey: "",
        playedAlertKeys: new Map(),
        ws: null,
        style: { ...DEFAULT_STYLE },
        controlsVisible: false,
    };

    const style = document.createElement("style");
    style.textContent = `
        :root {
            --cg-bg: #000000;
            --cg-text: #f8f8f8;
            --cg-accent: #c8102e;
            --cg-font: "Courier New", Courier, monospace;
            --cg-font-size: 56px;
            --cg-font-weight: 700;
            --cg-speed: 18s;
            --cg-gap: 4rem;
            --cg-shadow: 0 0 12px rgba(0, 0, 0, 0.7);
        }

        html, body {
            width: 100%;
            margin: 0;
            overflow: hidden;
            background: var(--cg-bg);
        }

        #chargen {
            position: fixed;
            inset: 0;
            width: 100vw;
            margin: 0;
            padding: 0;
            max-width: none;
            display: flex;
            align-items: center;
            justify-content: center;
            background: var(--cg-bg);
        }

        #cgControls {
            position: fixed;
            top: 12px;
            left: 12px;
            z-index: 100;
            display: inline-flex;
            flex-direction: column;
            align-items: flex-start;
            gap: 8px;
            font-family: "Segoe UI", Tahoma, sans-serif;
        }

        #cgControlsToggle,#backToDashboard {
            border: 1px solid rgba(255, 255, 255, 0.25);
            background: rgba(0, 0, 0, 0.7);
            color: #ffffff;
            font-size: 13px;
            cursor: pointer;
            border-radius: 6px;
            padding: 8px 10px;
        }

        #cgControlsForm {
            border: 1px solid rgba(255, 255, 255, 0.2);
            background: rgba(0, 0, 0, 0.8);
            color: #ffffff;
            border-radius: 8px;
            display: grid;
            grid-template-columns: 120px 180px;
            gap: 6px 8px;
            padding: 10px;
            align-items: center;
        }

        #cgControlsForm[hidden] {
            display: none;
        }

        #cgControlsForm label {
            font-size: 12px;
            opacity: 0.9;
        }

        #cgControlsForm input[type="text"],
        #cgControlsForm input[type="number"],
        #cgControlsForm input[type="color"] {
            width: 100%;
            box-sizing: border-box;
            border: 1px solid rgba(255, 255, 255, 0.25);
            border-radius: 4px;
            padding: 4px 6px;
            background: #101010;
            color: #ffffff;
        }

        #cgControlsForm input[type="checkbox"] {
            justify-self: start;
        }

        #cgResetStyle {
            grid-column: 1 / span 2;
            border: 1px solid rgba(255, 255, 255, 0.25);
            background: rgba(255, 255, 255, 0.12);
            color: #ffffff;
            border-radius: 6px;
            padding: 8px 10px;
            cursor: pointer;
        }

        #cgFrame {
            position: relative;
            width: 100vw;
            display: flex;
            align-items: center;
            justify-content: flex-start;
            background: linear-gradient(
                to bottom,
                rgba(0, 0, 0, 0.2),
                rgba(0, 0, 0, 0.55)
            );
        }

        #cgFrame::before,
        #cgFrame::after {
            content: "";
            position: absolute;
            left: 0;
            right: 0;
            height: 6px;
            background: linear-gradient(90deg, transparent, var(--cg-accent), transparent);
            opacity: 0.8;
            pointer-events: none;
        }

        #cgFrame::before { top: 0; }
        #cgFrame::after { bottom: 0; }

        #cgViewport {
            width: 100vw;
            flex: 0 0 100vw;
            max-width: 100vw;
            overflow: hidden;
        }

        #cgTrack {
            display: flex;
            width: max-content;
            white-space: nowrap;
            min-width: 0;
            will-change: transform;
            animation: cg-scroll var(--cg-speed) linear infinite;
            color: var(--cg-text);
            font-family: var(--cg-font);
            font-size: var(--cg-font-size);
            font-weight: var(--cg-font-weight);
            text-transform: uppercase;
            text-shadow: var(--cg-shadow);
            line-height: 1.2;
            letter-spacing: 0.06em;
        }

        #cgTrack > span {
            display: inline-block;
            flex: none;
            padding-right: var(--cg-gap);
        }

        #flex {
            display: inline-flex;
            gap: 8px;
        }

        @keyframes cg-scroll {
            from { transform: translateX(0); }
            to { transform: translateX(-50%); }
        }
    `;
    document.head.appendChild(style);

    function pickColor(raw) {
        if (!raw) return "";
        const value = String(raw).trim();
        return /^#([0-9a-f]{6})$/i.test(value) ? value : "";
    }

    function clampNumber(raw, min, max, fallback) {
        const num = Number(raw);
        if (!Number.isFinite(num)) return fallback;
        return Math.min(max, Math.max(min, num));
    }

    function normalizeStyleSettings(input) {
        const settings = typeof input === "object" && input ? input : {};
        const normalized = { ...DEFAULT_STYLE };

        const bgColor = pickColor(settings.bgColor);
        const textColor = pickColor(settings.textColor);
        const accentColor = pickColor(settings.accentColor);
        if (bgColor) normalized.bgColor = bgColor;
        if (textColor) normalized.textColor = textColor;
        if (accentColor) normalized.accentColor = accentColor;

        const fontFamily = String(settings.fontFamily || "").trim();
        if (fontFamily && fontFamily.length <= 160) normalized.fontFamily = fontFamily;

        const textShadow = String(settings.textShadow || "").trim();
        if (textShadow.length <= 200) normalized.textShadow = textShadow || DEFAULT_STYLE.textShadow;

        normalized.fontSize = clampNumber(settings.fontSize, 14, 180, DEFAULT_STYLE.fontSize);
        normalized.fontWeight = Math.round(clampNumber(settings.fontWeight, 100, 900, DEFAULT_STYLE.fontWeight));
        normalized.speed = clampNumber(settings.speed, 4, 120, DEFAULT_STYLE.speed);
        normalized.gap = clampNumber(settings.gap, 1, 300, DEFAULT_STYLE.gap);
        normalized.uppercase = Boolean(settings.uppercase);

        return normalized;
    }

    function applyStyleSettings(nextStyle) {
        const normalized = normalizeStyleSettings(nextStyle);
        state.style = normalized;
        const styleTarget = document.documentElement.style;
        styleTarget.setProperty("--cg-bg", normalized.bgColor);
        styleTarget.setProperty("--cg-text", normalized.textColor);
        styleTarget.setProperty("--cg-accent", normalized.accentColor);
        styleTarget.setProperty("--cg-font", normalized.fontFamily);
        styleTarget.setProperty("--cg-font-size", `${normalized.fontSize}px`);
        styleTarget.setProperty("--cg-font-weight", `${normalized.fontWeight}`);
        styleTarget.setProperty("--cg-speed", `${normalized.speed}s`);
        styleTarget.setProperty("--cg-gap", `${normalized.gap}rem`);
        styleTarget.setProperty("--cg-shadow", normalized.textShadow);
        track.style.textTransform = normalized.uppercase ? "uppercase" : "none";
    }

    function saveLocalStorage(key, value) {
        try {
            window.localStorage.setItem(key, JSON.stringify(value));
        } catch (_error) {
            // ignore storage failures
        }
    }

    function loadLocalStorage(key) {
        try {
            const raw = window.localStorage.getItem(key);
            if (!raw) return null;
            return JSON.parse(raw);
        } catch (_error) {
            return null;
        }
    }

    function syncControls(styleSettings) {
        if (!controls.form) return;
        controls.bgColor.value = styleSettings.bgColor;
        controls.textColor.value = styleSettings.textColor;
        controls.accentColor.value = styleSettings.accentColor;
        controls.fontFamily.value = styleSettings.fontFamily;
        controls.fontSize.value = String(styleSettings.fontSize);
        controls.fontWeight.value = String(styleSettings.fontWeight);
        controls.speed.value = String(styleSettings.speed);
        controls.gap.value = String(styleSettings.gap);
        controls.textShadow.value = styleSettings.textShadow;
        controls.uppercase.checked = Boolean(styleSettings.uppercase);
    }

    function readControlsToStyle() {
        return normalizeStyleSettings({
            bgColor: controls.bgColor.value,
            textColor: controls.textColor.value,
            accentColor: controls.accentColor.value,
            fontFamily: controls.fontFamily.value,
            fontSize: controls.fontSize.value,
            fontWeight: controls.fontWeight.value,
            speed: controls.speed.value,
            gap: controls.gap.value,
            textShadow: controls.textShadow.value,
            uppercase: controls.uppercase.checked,
        });
    }

    function updateControlsVisibility(visible) {
        state.controlsVisible = Boolean(visible);
        if (controls.form) controls.form.hidden = !state.controlsVisible;
        if (controls.toggle) {
            controls.toggle.textContent = state.controlsVisible ? "Hide Options" : "Show Options";
            controls.toggle.setAttribute("aria-expanded", state.controlsVisible ? "true" : "false");
        }
        saveLocalStorage(UI_STORAGE_KEY, { controlsVisible: state.controlsVisible });
    }

    function bindControls() {
        if (
            !controls.panel || !controls.toggle || !controls.form || !controls.bgColor || !controls.textColor
            || !controls.accentColor || !controls.fontFamily || !controls.fontSize || !controls.fontWeight
            || !controls.speed || !controls.gap || !controls.textShadow || !controls.uppercase || !controls.reset
        ) {
            return;
        }

        const onStyleInput = () => {
            const nextStyle = readControlsToStyle();
            applyStyleSettings(nextStyle);
            saveLocalStorage(STYLE_STORAGE_KEY, state.style);
        };

        const styleInputs = [
            controls.bgColor,
            controls.textColor,
            controls.accentColor,
            controls.fontFamily,
            controls.fontSize,
            controls.fontWeight,
            controls.speed,
            controls.gap,
            controls.textShadow,
            controls.uppercase,
        ];

        for (const input of styleInputs) {
            input.addEventListener("input", onStyleInput);
            input.addEventListener("change", onStyleInput);
        }

        controls.toggle.addEventListener("click", () => {
            updateControlsVisibility(!state.controlsVisible);
        });

        controls.reset.addEventListener("click", () => {
            applyStyleSettings(DEFAULT_STYLE);
            syncControls(state.style);
            saveLocalStorage(STYLE_STORAGE_KEY, state.style);
        });

        const savedStyle = normalizeStyleSettings(loadLocalStorage(STYLE_STORAGE_KEY) || DEFAULT_STYLE);
        const savedUi = loadLocalStorage(UI_STORAGE_KEY) || {};
        applyStyleSettings(savedStyle);
        syncControls(state.style);
        updateControlsVisibility(Boolean(savedUi.controlsVisible));
    }

    function setTickerText(rawText) {
        const text = (rawText || "").trim() || defaultText;
        if (state.activeAlertText === text) return;
        state.activeAlertText = text;
        renderTickerText(text);
    }

    function renderTickerText(baseText) {
        const seed = String(baseText || defaultText).replace(/\s+/g, " ").trim() || defaultText;
        const separator = "   ";
        let row = seed;
        textPrimary.textContent = row;

        const minWidth = Math.max(window.innerWidth + 120, viewport.clientWidth + 120, 800);
        let guard = 0;
        while (textPrimary.scrollWidth < minWidth && guard < 128) {
            row += `${separator}${seed}`;
            textPrimary.textContent = row;
            guard += 1;
        }

        textPrimary.textContent = row;
        textClone.textContent = row;
    }

    function alertKey(alert) {
        return `${alert?.received_at || ""}:${alert?.raw_header || ""}:${alert?.data?.event_code || ""}`;
    }

    function getLatestAlert(alerts) {
        if (!Array.isArray(alerts) || !alerts.length) return null;
        return alerts.reduce((latest, current) => {
            const latestTs = Number(latest?.received_at || 0);
            const currentTs = Number(current?.received_at || 0);
            return currentTs > latestTs ? current : latest;
        }, alerts[0]);
    }

    function clearAudio() {
        state.audioAttemptVersion += 1;
        if (state.audioRetryTimer !== null) {
            clearTimeout(state.audioRetryTimer);
            state.audioRetryTimer = null;
        }
        state.audioPendingAlertKey = "";
        audio.pause();
        audio.removeAttribute("src");
        audio.load();
        state.currentAudioSrc = "";
    }

    async function fetchLatestRecordingId() {
        try {
            const headers = token ? { Authorization: `Bearer ${token}` } : {};
            const response = await fetch("archive.php?latest_id=true", {
                method: "GET",
                headers,
                cache: "no-store",
            });
            if (!response.ok) return null;
            const text = await response.text();
            const id = Number.parseInt(text.trim(), 10);
            return Number.isInteger(id) && id >= 0 ? id : null;
        } catch (_error) {
            return null;
        }
    }

    async function isRecordingReady(recordingId) {
        if (!Number.isInteger(recordingId) || recordingId < 0) return false;
        try {
            const headers = token ? { Authorization: `Bearer ${token}` } : {};
            const response = await fetch(`archive.php?recording_id=${recordingId}`, {
                method: "HEAD",
                headers,
                cache: "no-store",
            });
            return response.ok;
        } catch (_error) {
            return false;
        }
    }

    function markAlertAudioPlayed(alertKey) {
        state.playedAlertKeys.set(alertKey, Date.now());
        if (state.playedAlertKeys.size <= 512) return;

        const keysByAge = Array.from(state.playedAlertKeys.entries())
            .sort((a, b) => a[1] - b[1])
            .map(([key]) => key);
        while (state.playedAlertKeys.size > 384 && keysByAge.length) {
            const key = keysByAge.shift();
            if (key) state.playedAlertKeys.delete(key);
        }
    }

    function scheduleAudioRetry(attemptVersion) {
        if (!state.audioPendingAlertKey) return;
        const pendingKey = state.audioPendingAlertKey;
        if (state.audioRetryTimer !== null) clearTimeout(state.audioRetryTimer);
        state.audioRetryTimer = setTimeout(() => {
            if (attemptVersion !== state.audioAttemptVersion) return;
            if (pendingKey !== state.audioPendingAlertKey) return;
            playLatestAlertAudio();
        }, 1500);
    }

    async function playLatestAlertAudio() {
        const pendingKey = state.audioPendingAlertKey;
        if (!pendingKey) return;
        if (state.playedAlertKeys.has(pendingKey)) {
            state.audioPendingAlertKey = "";
            return;
        }

        const attemptVersion = ++state.audioAttemptVersion;
        if (state.audioRetryTimer !== null) {
            clearTimeout(state.audioRetryTimer);
            state.audioRetryTimer = null;
        }

        const latestRecordingId = await fetchLatestRecordingId();
        if (attemptVersion !== state.audioAttemptVersion || pendingKey !== state.audioPendingAlertKey) return;

        if (latestRecordingId === null) {
            scheduleAudioRetry(attemptVersion);
            return;
        }

        const ready = await isRecordingReady(latestRecordingId);
        if (attemptVersion !== state.audioAttemptVersion || pendingKey !== state.audioPendingAlertKey) return;
        if (!ready) {
            scheduleAudioRetry(attemptVersion);
            return;
        }

        const src = `archive.php?recording_id=${latestRecordingId}`;
        if (src !== state.currentAudioSrc) {
            audio.src = src;
            audio.load();
            state.currentAudioSrc = src;
        }

        try {
            audio.currentTime = 0;
            await audio.play();
            if (pendingKey === state.audioPendingAlertKey) {
                markAlertAudioPlayed(pendingKey);
                state.audioPendingAlertKey = "";
            }
        } catch (_error) {
            scheduleAudioRetry(attemptVersion);
        }
    }

    function applyLatestAlert(alerts) {
        const latest = getLatestAlert(alerts);
        if (!latest) {
            state.activeAlertKey = "";
            setTickerText(defaultText);
            clearAudio();
            return;
        }

        const key = alertKey(latest);
        const text = latest?.data?.eas_text || defaultText;

        if (key === state.activeAlertKey) {
            setTickerText(text);
            return;
        }

        state.activeAlertKey = key;
        setTickerText(text);
        clearAudio();
        if (!state.playedAlertKeys.has(key)) {
            state.audioPendingAlertKey = key;
            playLatestAlertAudio();
        }
    }

    function handleWsMessage(event) {
        let payload;
        try {
            payload = JSON.parse(event.data);
        } catch (_error) {
            return;
        }

        if (!payload || typeof payload !== "object") return;

        if (payload.type === "Snapshot") {
            applyLatestAlert(payload.payload?.active_alerts || []);
            return;
        }

        if (payload.type === "Alerts") {
            applyLatestAlert(Array.isArray(payload.payload) ? payload.payload : []);
        }
    }

    async function fetchStatus() {
        try {
            const protocol = window.location.protocol === "https:" ? "https" : "http";
            const response = await fetch(`${protocol}://${apiBase}/api/status`, {
                headers: {
                    Accept: "application/json",
                    ...(token ? { Authorization: `Bearer ${token}` } : {}),
                },
            });
            if (!response.ok) return;
            const payload = await response.json();
            applyLatestAlert(payload?.active_alerts || []);
        } catch (_error) {
            applyLatestAlert([]);
        }
    }

    function scheduleReconnect() {
        if (state.reconnectTimer !== null) return;
        state.reconnectTimer = setTimeout(() => {
            state.reconnectTimer = null;
            connectWebSocket();
        }, state.reconnectDelayMs);
        state.reconnectDelayMs = Math.min(Math.round(state.reconnectDelayMs * 1.8), 30000);
    }

    function connectWebSocket() {
        const protocol = window.location.protocol === "https:" ? "wss" : "ws";
        const wsUrl = `${protocol}://${apiBase}/ws?auth=${encodeURIComponent(token)}`;

        try {
            state.ws = new WebSocket(wsUrl);
        } catch (_error) {
            scheduleReconnect();
            return;
        }

        state.ws.addEventListener("open", () => {
            state.reconnectDelayMs = 2000;
        });

        state.ws.addEventListener("message", handleWsMessage);

        state.ws.addEventListener("close", () => {
            scheduleReconnect();
        });

        state.ws.addEventListener("error", () => {
            try {
                state.ws.close();
            } catch (_error) {
                // ignore
            }
        });
    }

    document.addEventListener("visibilitychange", () => {
        if (!document.hidden && (!state.ws || state.ws.readyState === WebSocket.CLOSED)) {
            state.reconnectDelayMs = 2000;
            connectWebSocket();
        }
    });

    let resizeTimer = null;
    window.addEventListener("resize", () => {
        if (resizeTimer !== null) {
            clearTimeout(resizeTimer);
        }
        resizeTimer = setTimeout(() => {
            resizeTimer = null;
            renderTickerText(state.activeAlertText || defaultText);
        }, 120);
    });

    applyStyleSettings(DEFAULT_STYLE);
    bindControls();
    setTickerText(defaultText);
    fetchStatus().finally(connectWebSocket);
    setInterval(fetchStatus, 60000);
})();
