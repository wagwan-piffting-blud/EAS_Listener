(function () {
    const LOG_LIMIT = parseInt(window.MONITORING_MAX_LOGS, 10) || 500;
    const LOG_FETCH_TAIL = Math.min(LOG_LIMIT, 500);
    const AUDIO_AVAILABILITY_POLL_MS = 10000;
    const AUDIO_INITIAL_HOLDOFF_MS = 10000;
    const AUDIO_NOT_AVAILABLE_TEXT = "Audio is not currently available. Maybe it's still recording? Retrying in __SECOND__s...";

    const state = {
        streams: new Map(),
        activeAlerts: [],
        activeAlertSignature: "",
        activeAlertsChangedAt: 0,
        activeAlertAudioSrc: null,
        nextAudioAvailabilityCheckAt: 0,
        audioPollInFlight: false,
        logs: [],
    };
    let audioAvailabilityPollTimer = null;
    let audioUnavailableCountdownTimer = null;

    const elements = {
        wsStatus: document.getElementById("wsStatus"),
        streamGrid: document.getElementById("streamGrid"),
        streamCount: document.getElementById("streamCount"),
        alertList: document.getElementById("alertList"),
        alertCount: document.getElementById("alertCount"),
        logList: document.getElementById("logList"),
        logCount: document.getElementById("logCount"),
    };

    function setWsStatus(text, statusClass) {
        elements.wsStatus.textContent = text;
        elements.wsStatus.className = `ws-status ${statusClass || ""}`.trim();
    }

    function formatTimestamp(ts, withTime = true) {
        if (ts === null || ts === undefined) return "—";
        const date = new Date(ts);
        if (Number.isNaN(date.getTime())) return "—";
        const options = withTime
        ? {
            year: "numeric",
            month: "short",
            day: "numeric",
            hour: "2-digit",
            minute: "2-digit",
            second: "2-digit",
            }
        : {
            year: "numeric",
            month: "short",
            day: "numeric",
            };
        return new Intl.DateTimeFormat(undefined, options).format(date);
    }

    function formatDuration(seconds) {
        if (!seconds || seconds <= 0) return "—";
        const abs = Math.floor(seconds);
        const hrs = Math.floor(abs / 3600);
        const mins = Math.floor((abs % 3600) / 60);
        const secs = abs % 60;
        if (hrs > 0) {
            return `${hrs}h ${mins.toString().padStart(2, "0")}m`;
        }
        if (mins > 0) {
            return `${mins}m ${secs.toString().padStart(2, "0")}s`;
        }
        return `${secs}s`;
    }

    function applyStatusPayload(payload) {
        if (payload.streams) {
            state.streams.clear();
            payload.streams.forEach((stream) => {
                state.streams.set(stream.stream_url, stream);
            });
        }
        if (payload.active_alerts) {
            setActiveAlerts(payload.active_alerts);
        }
        renderStreams();
        renderAlerts();
    }

    function buildAlertSignature(alerts) {
        return alerts
            .map((alert) => `${alert.received_at || ""}:${alert.data?.event_code || ""}:${alert.raw_header || ""}`)
            .join("|");
    }

    function startAudioAvailabilityPolling() {
        if (audioAvailabilityPollTimer !== null) return;
        audioAvailabilityPollTimer = setInterval(checkForAvailableAlertAudio, AUDIO_AVAILABILITY_POLL_MS);
    }

    function stopAudioAvailabilityPolling() {
        if (audioAvailabilityPollTimer === null) return;
        clearInterval(audioAvailabilityPollTimer);
        audioAvailabilityPollTimer = null;
    }

    function getAudioUnavailableCountdownSeconds() {
        const targetAt = state.nextAudioAvailabilityCheckAt;
        if (!targetAt) return Math.round(AUDIO_AVAILABILITY_POLL_MS / 1000);
        return Math.max(0, Math.ceil((targetAt - Date.now()) / 1000));
    }

    function getAudioUnavailableText() {
        return AUDIO_NOT_AVAILABLE_TEXT.replace("__SECOND__", getAudioUnavailableCountdownSeconds());
    }

    function refreshAudioUnavailableCountdown() {
        const unavailable = getAudioUnavailableText();
        elements.alertList
            .querySelectorAll("[data-audio-unavailable='true']")
            .forEach((el) => {
                el.textContent = unavailable;
            });
    }

    function startAudioUnavailableCountdown() {
        if (audioUnavailableCountdownTimer !== null) return;
        refreshAudioUnavailableCountdown();
        audioUnavailableCountdownTimer = setInterval(refreshAudioUnavailableCountdown, 1000);
    }

    function stopAudioUnavailableCountdown() {
        if (audioUnavailableCountdownTimer === null) return;
        clearInterval(audioUnavailableCountdownTimer);
        audioUnavailableCountdownTimer = null;
    }

    function updateAudioAvailabilityPolling() {
        if (state.activeAlerts.length > 0 && !state.activeAlertAudioSrc) {
            if (!state.nextAudioAvailabilityCheckAt) {
                state.nextAudioAvailabilityCheckAt = Date.now() + AUDIO_INITIAL_HOLDOFF_MS;
            }
            startAudioAvailabilityPolling();
            startAudioUnavailableCountdown();
            return;
        }
        state.nextAudioAvailabilityCheckAt = 0;
        stopAudioAvailabilityPolling();
        stopAudioUnavailableCountdown();
    }

    function setActiveAlerts(alerts) {
        const nextAlerts = Array.isArray(alerts) ? alerts.slice() : [];
        const nextSignature = buildAlertSignature(nextAlerts);
        const changed = nextSignature !== state.activeAlertSignature;

        state.activeAlerts = nextAlerts;
        if (changed) {
            state.activeAlertSignature = nextSignature;
            state.activeAlertsChangedAt = Date.now();
            state.activeAlertAudioSrc = null;
            state.nextAudioAvailabilityCheckAt = nextAlerts.length
                ? state.activeAlertsChangedAt + AUDIO_INITIAL_HOLDOFF_MS
                : 0;
        }

        updateAudioAvailabilityPolling();
        if (changed && nextAlerts.length) {
            precheckAvailableAlertAudio();
        }

        return changed;
    }

    function applyLogs(logs) {
        if (!Array.isArray(logs)) return;
        const combined = [...logs, ...state.logs];
        combined.sort((a, b) => b.id - a.id);
        state.logs = combined.slice(0, LOG_LIMIT);
        renderLogs();
    }

    function renderStreams() {
        const container = elements.streamGrid;
        container.innerHTML = "";
        const streams = Array.from(state.streams.values()).sort((a, b) =>
            a.stream_url.localeCompare(b.stream_url)
        );
        elements.streamCount.textContent = `${streams.length} tracked`;

        if (!streams.length) {
            container.innerHTML = '<div class="empty-state">No streams configured.</div>';
            return;
        }

        for (const stream of streams) {
            const card = document.createElement("article");
            card.className = `stream-card ${stream.is_connected ? "online" : "offline"}`;

            const receivingText = stream.is_receiving_audio
                ? "Receiving audio"
                : "No audio activity";
            const statusLabel = stream.is_connected ? "Connected" : "Disconnected";
            const uptime = stream.uptime_seconds
                ? formatDuration(stream.uptime_seconds)
                : "—";

            const lastActivity = stream.last_activity
                ? formatTimestamp(stream.last_activity * 1000)
                : "Never";

            const lastDisconnect = stream.last_disconnect
                ? formatTimestamp(stream.last_disconnect * 1000)
                : "—";

            const connectedSince = stream.connected_since
                ? formatTimestamp(stream.connected_since * 1000)
                : "—";

            card.innerHTML = `
                <div class="stream-header">
                <div class="status-tag">${statusLabel}</div>
                <div class="stream-url">${stream.stream_url}</div>
                </div>
                <div class="stream-meta">
                    <div><strong>Audio:</strong> ${receivingText}</div>
                    <div><strong>Uptime:</strong> ${uptime}</div>
                    <div><strong>Connected since:</strong> ${connectedSince}</div>
                    <div><strong>Last audio:</strong> ${lastActivity}</div>
                    <div><strong>Last disconnect:</strong> ${lastDisconnect}</div>
                    <div><strong>Attempts:</strong> ${stream.connection_attempts}</div>
                    <div><strong>Last error:</strong> ${stream.last_error || "—"}</div>
                </div>
            `;
            container.appendChild(card);
        }
    }

    function secondsToHM(totalSeconds) {
        if (totalSeconds < 0 || isNaN(totalSeconds)) {
            return "Invalid input";
        }

        const hours = Math.floor(totalSeconds / 3600);
        const minutes = Math.floor((totalSeconds % 3600) / 60);

        const hoursPart = hours > 0 ? `${hours}h` : '';
        const minutesPart = minutes >= 0 ? `${minutes}m` : '';

        if (hours === 0 && minutes === 0) {
            return "0m";
        }

        return `${hoursPart}${hoursPart && minutesPart ? ' ' : ''}${minutesPart}`;
    }

    function fetch_audio(src) {
        if (!src) {
            return `<span data-audio-unavailable="true">${getAudioUnavailableText()}</span>`;
        }
        return `<audio controls preload="metadata" data-alert-audio="true"><source src="${src}" type="audio/wav">Your browser does not support the audio element.</audio>`;
    }

    async function fetchAudioUrl() {
        try {
            const response = await fetch(`/archive.php?latest_id=true`, {
                headers: {
                    Authorization: `Bearer ${window.TOKEN}`,
                },
            });
            if (!response.ok) {
                throw new Error(`HTTP ${response.status}`);
            }
            const text = await response.text();
            const id = parseInt(text.trim(), 10);
            if (isNaN(id) || id < 0) return null;
            return id;
        } catch (err) {
            console.error("Failed to fetch latest recording ID", err);
            return null;
        }
    }

    async function isAudioAvailable(src) {
        const headers = {
            Authorization: `Bearer ${window.TOKEN}`,
            Range: "bytes=44-45",
        };

        try {
            const response = await fetch(`/${src}`, {
                method: "GET",
                headers,
                cache: "no-store",
            });

            if (!response.ok && response.status !== 206) return false;

            const contentType = (response.headers.get("content-type") || "").toLowerCase();
            if (
                contentType &&
                (contentType.startsWith("text/") ||
                contentType.includes("html") ||
                contentType.includes("json"))
            ) {
                return false;
            }

            const chunk = await response.arrayBuffer();
            if (chunk.byteLength <= 0) return false;

            return true;
        } catch (err) {
            return false;
        }
    }

    async function isAudioPlayable(src, timeoutMs = 5000) {
        return await new Promise((resolve) => {
            const audio = document.createElement("audio");
            let settled = false;

            const finish = (ok) => {
                if (settled) return;
                settled = true;
                clearTimeout(timer);
                audio.removeEventListener("loadedmetadata", onLoadedMetadata);
                audio.removeEventListener("canplay", onCanPlay);
                audio.removeEventListener("error", onError);
                audio.pause();
                audio.removeAttribute("src");
                audio.load();
                resolve(ok);
            };

            const onLoadedMetadata = () => {
                const duration = audio.duration;
                finish(Number.isFinite(duration) && duration > 0);
            };
            const onCanPlay = () => finish(true);
            const onError = () => finish(false);

            const timer = setTimeout(() => finish(false), timeoutMs);

            audio.preload = "metadata";
            audio.addEventListener("loadedmetadata", onLoadedMetadata, { once: true });
            audio.addEventListener("canplay", onCanPlay, { once: true });
            audio.addEventListener("error", onError, { once: true });
            audio.src = src;
            audio.load();
        });
    }

    async function checkForAvailableAlertAudio() {
        if (!state.activeAlerts.length || state.activeAlertAudioSrc || state.audioPollInFlight) {
            updateAudioAvailabilityPolling();
            return;
        }
        if (
            state.activeAlertsChangedAt &&
            Date.now() - state.activeAlertsChangedAt < AUDIO_INITIAL_HOLDOFF_MS
        ) {
            state.nextAudioAvailabilityCheckAt = state.activeAlertsChangedAt + AUDIO_INITIAL_HOLDOFF_MS;
            refreshAudioUnavailableCountdown();
            return;
        }

        state.nextAudioAvailabilityCheckAt = Date.now() + AUDIO_AVAILABILITY_POLL_MS;
        refreshAudioUnavailableCountdown();
        state.audioPollInFlight = true;
        try {
            const alertSignatureAtStart = state.activeAlertSignature;
            const availableAudioSrc = await fetchAvailableAudioSrc(1, 0);
            if (!availableAudioSrc) return;

            const playable = await isAudioPlayable(availableAudioSrc);
            if (!playable) return;
            if (state.activeAlertSignature !== alertSignatureAtStart) return;

            if (availableAudioSrc !== state.activeAlertAudioSrc) {
                state.activeAlertAudioSrc = availableAudioSrc;
                renderAlerts();
                updateAudioAvailabilityPolling();
            }
        } finally {
            state.audioPollInFlight = false;
        }
    }

    async function precheckAvailableAlertAudio() {
        if (!state.activeAlerts.length || state.activeAlertAudioSrc || state.audioPollInFlight) {
            return;
        }

        state.audioPollInFlight = true;
        try {
            const alertSignatureAtStart = state.activeAlertSignature;
            const availableAudioSrc = await fetchAvailableAudioSrc(1, 0);
            if (!availableAudioSrc) return;

            const playable = await isAudioPlayable(availableAudioSrc);
            if (!playable) return;
            if (state.activeAlertSignature !== alertSignatureAtStart) return;

            if (availableAudioSrc !== state.activeAlertAudioSrc) {
                state.activeAlertAudioSrc = availableAudioSrc;
                renderAlerts();
                updateAudioAvailabilityPolling();
            }
        } finally {
            state.audioPollInFlight = false;
        }
    }

    function bindAudioUnavailableFallback(card) {
        const audioEl = card.querySelector("audio[data-alert-audio='true']");
        if (!audioEl) return;
        const sourceEl = audioEl.querySelector("source");
        const failedSrc = sourceEl?.getAttribute("src") || "";

        const showUnavailable = () => {
            const unavailable = document.createElement("span");
            unavailable.dataset.audioUnavailable = "true";
            state.nextAudioAvailabilityCheckAt = Date.now() + AUDIO_AVAILABILITY_POLL_MS;
            unavailable.textContent = getAudioUnavailableText();
            audioEl.replaceWith(unavailable);
            startAudioUnavailableCountdown();
            if (failedSrc && state.activeAlertAudioSrc === failedSrc) {
                state.activeAlertAudioSrc = null;
                updateAudioAvailabilityPolling();
            }
        };

        audioEl.addEventListener("error", showUnavailable, { once: true });
        if (sourceEl) {
            sourceEl.addEventListener("error", showUnavailable, { once: true });
        }
    }

    async function fetchAvailableAudioSrc(maxAttempts = 4, delayMs = 600) {
        for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
            const latestRecordingId = await fetchAudioUrl();
            if (latestRecordingId !== null) {
                const src = `archive.php?recording_id=${latestRecordingId}`;
                if (await isAudioAvailable(src)) {
                    return src;
                }
            }

            if (attempt < maxAttempts - 1) {
                await new Promise((resolve) => setTimeout(resolve, delayMs));
            }
        }

        return null;
    }

    function renderAlerts() {
        const container = elements.alertList;
        container.innerHTML = "";
        const alerts = state.activeAlerts.slice().sort((a, b) =>
            b.received_at - a.received_at
        );
        elements.alertCount.textContent = alerts.length ? `${alerts.length} active` : "None";

        if (!alerts.length) {
            container.innerHTML = '<div class="empty-state">No active alerts.</div>';
            return;
        }

        alerts.forEach((alert, index) => {
            const card = document.createElement("article");
            const severity = RegExp(/(warning|watch|advisory|emergency|test)/i).exec(alert.data.event_text)?.[1]?.toLowerCase();
            var availableAudioSrc = state.activeAlertAudioSrc;
            availableAudioSrc = availableAudioSrc.replace(/(\d+)$/, function(match, p1) {
                return (parseInt(p1, 10) - index).toString();
            });

            card.className = `alert-card ${severity || "unknown"}`;
            card.innerHTML = `
                <div class="event-code">${alert.data.event_code}</div>
                <div class="headline">${alert.data.event_text}</div>
                <div class="meta">
                    <div>${alert.data.eas_text || "Alert received."}</div>
                    <br>
                    <div><strong>Originator:</strong> ${alert.data.originator}</div>
                    <br>
                    <div><strong>Severity:</strong> ${severity.toUpperCase()}</div>
                    <br>
                    <div><strong>Locations:</strong> ${alert.data.locations || "—"}</div>
                    <br>
                    <div><strong>Received:</strong> ${formatTimestamp(alert.received_at * 1000)}</div>
                    <br>
                    <div><strong>Expires:</strong> ${formatTimestamp(alert.expires_at * 1000)}</div>
                    <br>
                    <div><strong>Length:</strong> ${alert.purge_time.secs ? secondsToHM(alert.purge_time.secs) : "—"}</div>
                    <br>
                    <div><strong>Raw ZCZC String:</strong> <pre>${alert.raw_header || "—"}</pre></div>
                    <br>
                    <div><strong>Recording audio:&ensp;</strong> ${fetch_audio(availableAudioSrc)}</div>
                </div>
            `;

            container.appendChild(card);
            bindAudioUnavailableFallback(card);
        });
    }

    function renderLogs() {
        const container = elements.logList;
        container.innerHTML = "";
        const logs = state.logs;
        elements.logCount.textContent = `${logs.length} entries`;

        if (!logs.length) {
            container.innerHTML = '<div class="empty-state">No log entries captured yet.</div>';
            return;
        }

        for (const log of logs) {
            const entry = document.createElement("article");
            entry.className = "log-entry";
            entry.dataset.level = log.level || "INFO";

            const time = formatTimestamp(log.timestamp);
            const fields = Object.keys(log.fields || {}).length
                ? JSON.stringify(log.fields, null, 2)
                : "";

            entry.innerHTML = `
                <div class="log-meta">
                    <span>${log.level}</span>
                    <span>${time}</span>
                    <span>${log.target}</span>
                </div>
                <div class="log-message">${log.message || ""}</div>
                ${fields ? `<pre>${fields}</pre>` : ""}
            `;
            container.appendChild(entry);
        }
    }

    async function fetchJson(path) {
        try {
            const protocol = window.location.protocol === "https:" ? "https" : "http";
            const response = await fetch(`${protocol}://${window.API_BASE}${path}`, {
                headers: {
                    Accept: "application/json",
                    Authorization: `Bearer ${window.TOKEN}`,
                },
            });
            if (!response.ok) {
                throw new Error(`HTTP ${response.status}`);
            }
            return await response.json();
        } catch (err) {
            console.error(`Failed to fetch ${path}:`, err);
            return null;
        }
    }

    async function loadInitialData() {
        const [statusResponse, logResponse] = await Promise.all([
            fetchJson(`/api/status`),
            fetchJson(`/api/logs?tail=${LOG_FETCH_TAIL}`),
        ]);

        if (statusResponse) {
            applyStatusPayload(statusResponse);
        }
        if (logResponse && Array.isArray(logResponse.logs)) {
            state.logs = logResponse.logs
                .slice()
                .sort((a, b) => b.id - a.id)
                .slice(0, LOG_LIMIT);
            renderLogs();
        }
    }

    function handleWsMessage(event) {
        try {
            const payload = JSON.parse(event.data);
            if (!payload || typeof payload !== "object") return;

            switch (payload.type) {
                case "Snapshot":
                    applyStatusPayload(payload.payload);
                    if (Array.isArray(payload.payload.logs)) {
                        state.logs = payload.payload.logs
                        .slice()
                        .sort((a, b) => b.id - a.id)
                        .slice(0, LOG_LIMIT);
                        renderLogs();
                    }
                    break;
                case "Stream":
                    if (payload.payload?.stream_url) {
                        state.streams.set(payload.payload.stream_url, payload.payload);
                        renderStreams();
                    }
                    break;
                case "Log":
                    if (payload.payload) {
                        applyLogs([payload.payload]);
                    }
                    break;
                case "Alerts":
                    if (Array.isArray(payload.payload)) {
                        setActiveAlerts(payload.payload);
                        renderAlerts();
                    }
                    break;
                default:
                    console.warn("Unhandled WS message type", payload.type);
            }
        } catch (err) {
            console.error("Failed to parse WS message", err);
        }
    }

    let ws;
    let reconnectDelay = 2000;
    const MAX_DELAY = 30000;

    function connectWebSocket() {
        const protocol = window.location.protocol === "https:" ? "wss" : "ws";
        const url = `${protocol}://${window.API_BASE}/ws?auth=${encodeURIComponent(window.TOKEN)}`;
        setWsStatus("Connecting...", "");

        try {
            ws = new WebSocket(url);
        } catch (err) {
            console.error("WebSocket init failed", err);
            scheduleReconnect();
            return;
        }

        ws.addEventListener("open", () => {
            setWsStatus("Live updates", "connected");
            reconnectDelay = 2000;
        });

        ws.addEventListener("message", handleWsMessage);

        ws.addEventListener("close", () => {
            setWsStatus("Disconnected", "disconnected");
            scheduleReconnect();
        });

        ws.addEventListener("error", (err) => {
            console.error("WebSocket error", err);
            ws.close();
        });
    }

    function scheduleReconnect() {
        setWsStatus(`Reconnecting in ${Math.round(reconnectDelay / 1000)}s...`, "reconnecting");
        setTimeout(connectWebSocket, reconnectDelay);
        reconnectDelay = Math.min(reconnectDelay * 1.8, MAX_DELAY);
    }

    function bootstrap() {
        loadInitialData().finally(connectWebSocket);
        setInterval(loadInitialData, 60000);
        updateAudioAvailabilityPolling();
    }

    document.addEventListener("visibilitychange", () => {
        if (!document.hidden && (!ws || ws.readyState === WebSocket.CLOSED)) {
            reconnectDelay = 2000;
            connectWebSocket();
        }
    });

    bootstrap();
})();
