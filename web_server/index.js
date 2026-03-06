(function () {
    const LOG_LIMIT = parseInt(window.MONITORING_MAX_LOGS, 10) || 500;
    const LOG_FETCH_TAIL = Math.min(LOG_LIMIT, 500);
    const AUDIO_AVAILABILITY_POLL_MS = 10000;
    const AUDIO_INITIAL_HOLDOFF_MS = 10000;
    const AUDIO_PROBE_BATCH_SIZE = 4;
    const AUDIO_PROBE_CONCURRENCY = 2;
    const AUDIO_PROBE_BACKOFF_BASE_MS = 3000;
    const AUDIO_PROBE_BACKOFF_MAX_MS = 60000;
    const AUDIO_NOT_AVAILABLE_TEXT = "Audio is not currently available. Maybe it's still recording? Retrying in __SECOND__s...";
    const STREAM_RENDER_FALLBACK_DELAY_MS = 16;
    const TIMESTAMP_WITH_TIME_FORMATTER = new Intl.DateTimeFormat(undefined, {
        year: "numeric",
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
    });
    const TIMESTAMP_DATE_ONLY_FORMATTER = new Intl.DateTimeFormat(undefined, {
        year: "numeric",
        month: "short",
        day: "numeric",
    });
    const LOCATION_CODE_PATTERN = /\d{6}/g;
    const LOCATION_COUNTY_PATTERN = /\bCounty\b(?=,|$)/gi;
    const STATE_AND_TERRITORY_NAMES = Object.freeze({
        AL: "Alabama",
        AK: "Alaska",
        AZ: "Arizona",
        AR: "Arkansas",
        CA: "California",
        CO: "Colorado",
        CT: "Connecticut",
        DE: "Delaware",
        FL: "Florida",
        GA: "Georgia",
        HI: "Hawaii",
        ID: "Idaho",
        IL: "Illinois",
        IN: "Indiana",
        IA: "Iowa",
        KS: "Kansas",
        KY: "Kentucky",
        LA: "Louisiana",
        ME: "Maine",
        MD: "Maryland",
        MA: "Massachusetts",
        MI: "Michigan",
        MN: "Minnesota",
        MS: "Mississippi",
        MO: "Missouri",
        MT: "Montana",
        NE: "Nebraska",
        NV: "Nevada",
        NH: "New Hampshire",
        NJ: "New Jersey",
        NM: "New Mexico",
        NY: "New York",
        NC: "North Carolina",
        ND: "North Dakota",
        OH: "Ohio",
        OK: "Oklahoma",
        OR: "Oregon",
        PA: "Pennsylvania",
        RI: "Rhode Island",
        SC: "South Carolina",
        SD: "South Dakota",
        TN: "Tennessee",
        TX: "Texas",
        UT: "Utah",
        VT: "Vermont",
        VA: "Virginia",
        WA: "Washington",
        WV: "West Virginia",
        WI: "Wisconsin",
        WY: "Wyoming",
        DC: "District of Columbia",
        AS: "American Samoa",
        GU: "Guam",
        MP: "Northern Mariana Islands",
        PR: "Puerto Rico",
        VI: "U.S. Virgin Islands",
        UM: "U.S. Minor Outlying Islands",
    });
    const NEW_ALERT_SOUND_SRC = window.ALERTSOUNDDATA || "";
    const NEW_ALERT_SOUND_ENABLED = window.ALERTSOUNDENABLED === true;

    const state = {
        streams: new Map(),
        activeAlerts: [],
        activeAlertSignature: "",
        activeAlertsChangedAt: 0,
        activeAlertAudioSrcByAlert: new Map(),
        audioProbeStateByRecordingId: new Map(),
        nextAudioAvailabilityCheckAt: 0,
        audioPollInFlight: false,
        sameUsByFips: null,
        sameUsSubdivByCode: null,
        sameUsLoadPromise: null,
        locationLabelCache: new Map(),
        logs: [],
        capStatus: null,
        streamCardByUrl: new Map(),
        pendingStreamUrls: new Set(),
        streamRenderScheduled: false,
    };
    let audioAvailabilityPollTimer = null;
    let audioUnavailableCountdownTimer = null;
    let newAlertSound = null;

    const elements = {
        wsStatus: document.getElementById("wsStatus"),
        streamGrid: document.getElementById("streamGrid"),
        streamCount: document.getElementById("streamCount"),
        alertList: document.getElementById("alertList"),
        alertCount: document.getElementById("alertCount"),
        logList: document.getElementById("logList"),
        logCount: document.getElementById("logCount"),
        capStatusSection: document.getElementById("capStatusSection"),
        capStatus: document.getElementById("capStatus"),
    };

    function setWsStatus(text, statusClass) {
        elements.wsStatus.textContent = text;
        elements.wsStatus.className = `ws-status ${statusClass || ""}`.trim();
    }

    function formatTimestamp(ts, withTime = true) {
        if (ts === null || ts === undefined) return "-";
        const date = new Date(ts);
        if (Number.isNaN(date.getTime())) return "-";
        return (withTime ? TIMESTAMP_WITH_TIME_FORMATTER : TIMESTAMP_DATE_ONLY_FORMATTER).format(date);
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

    function escapeHtml(value) {
        return String(value ?? "")
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")
            .replace(/>/g, "&gt;")
            .replace(/"/g, "&quot;")
            .replace(/'/g, "&#39;");
    }

    function expandStateAbbreviations(value) {
        if (!value) return "";
        return String(value).replace(/,\s*([A-Z]{2})\b/g, (fullMatch, code) => {
            const fullName = STATE_AND_TERRITORY_NAMES[code];
            return fullName ? `, ${fullName}` : fullMatch;
        });
    }

    function normalizeLocationSeparators(value) {
        const input = String(value || "").trim();
        if (!input) return "";
        if (input.includes(";")) {
            return input
                .split(";")
                .map((part) => part.trim())
                .filter(Boolean)
                .join("; ");
        }

        const parts = input
            .split(",")
            .map((part) => part.trim())
            .filter(Boolean);
        if (parts.length >= 4 && parts.length % 2 === 0) {
            const locations = [];
            for (let i = 0; i < parts.length; i += 2) {
                locations.push(`${parts[i]}, ${parts[i + 1]}`);
            }
            return locations.join("; ");
        }

        return input;
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
        if (payload.cap_status !== undefined) {
            state.capStatus = payload.cap_status;
        }
        renderStreams();
        renderAlerts();
        renderCapStatus();
    }

    function parsedHeaderForAlert(alert) {
        return alert?.data?.parsed_header || null;
    }

    function eventCodeForAlert(alert) {
        return parsedHeaderForAlert(alert)?.event_code || alert?.data?.event_code || "";
    }

    function originatorForAlert(alert) {
        return alert?.data?.originator || parsedHeaderForAlert(alert)?.originator || "";
    }

    function locationCodesForAlert(alert) {
        const parsed = parsedHeaderForAlert(alert);
        if (Array.isArray(parsed?.fips_codes) && parsed.fips_codes.length) {
            return parsed.fips_codes.join(", ");
        }
        return alert?.data?.locations || "";
    }

    function buildAlertSignature(alerts) {
        return alerts
            .map((alert) => `${alert.received_at || ""}:${eventCodeForAlert(alert)}:${alert.raw_header || ""}`)
            .join("|");
    }

    function getAlertKey(alert) {
        return `${alert.received_at || ""}:${eventCodeForAlert(alert)}:${alert.raw_header || ""}`;
    }

    function getSortedActiveAlerts(alerts = state.activeAlerts) {
        return alerts.slice().sort((a, b) => b.received_at - a.received_at);
    }

    function hasPendingAlertAudio() {
        return state.activeAlerts.some((alert) => !state.activeAlertAudioSrcByAlert.has(getAlertKey(alert)));
    }

    function shouldProbeRecordingId(recordingId, now = Date.now()) {
        const probeState = state.audioProbeStateByRecordingId.get(recordingId);
        return !probeState || probeState.nextTryAt <= now;
    }

    function markRecordingIdProbeFailure(recordingId) {
        const previous = state.audioProbeStateByRecordingId.get(recordingId) || { failures: 0, nextTryAt: 0 };
        const failures = previous.failures + 1;
        const backoffMs = Math.min(
            AUDIO_PROBE_BACKOFF_MAX_MS,
            AUDIO_PROBE_BACKOFF_BASE_MS * (2 ** (failures - 1))
        );
        state.audioProbeStateByRecordingId.set(recordingId, {
            failures,
            nextTryAt: Date.now() + backoffMs,
        });
    }

    function markRecordingIdProbeSuccess(recordingId) {
        state.audioProbeStateByRecordingId.delete(recordingId);
    }

    function pruneProbeState(latestRecordingId, alertCount) {
        if (state.audioProbeStateByRecordingId.size <= 256) return;
        const windowSize = Math.max(alertCount * 2, 32);
        const minRelevantRecordingId = Math.max(0, latestRecordingId - windowSize);
        state.audioProbeStateByRecordingId.forEach((_value, recordingId) => {
            if (recordingId < minRelevantRecordingId || recordingId > latestRecordingId) {
                state.audioProbeStateByRecordingId.delete(recordingId);
            }
        });
    }

    async function mapWithConcurrency(items, concurrency, task) {
        if (!items.length) return [];
        const results = new Array(items.length);
        const workerCount = Math.min(Math.max(1, concurrency), items.length);
        let nextIndex = 0;

        async function worker() {
            while (nextIndex < items.length) {
                const currentIndex = nextIndex;
                nextIndex += 1;
                results[currentIndex] = await task(items[currentIndex], currentIndex);
            }
        }

        await Promise.all(Array.from({ length: workerCount }, worker));
        return results;
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
        if (state.activeAlerts.length > 0 && hasPendingAlertAudio()) {
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

    function playNewAlertSound() {
        if (!NEW_ALERT_SOUND_SRC || !NEW_ALERT_SOUND_ENABLED) {
            return;
        }

        // Parse data uri to binary and play it as audio
        if (NEW_ALERT_SOUND_SRC.startsWith("data:audio")) {
            try {
                const mime_type = NEW_ALERT_SOUND_SRC.substring(5, NEW_ALERT_SOUND_SRC.indexOf(";base64"));
                const base64Data = NEW_ALERT_SOUND_SRC.split(";base64,").pop();
                const binaryData = atob(base64Data);
                const arrayBuffer = new ArrayBuffer(binaryData.length);
                const uint8Array = new Uint8Array(arrayBuffer);
                for (let i = 0; i < binaryData.length; i++) {
                    uint8Array[i] = binaryData.charCodeAt(i);
                }
                const blob = new Blob([arrayBuffer], { type: mime_type });
                const url = URL.createObjectURL(blob);
                const audio = new Audio(url);
                let revoked = false;
                const cleanup = () => {
                    if (revoked) return;
                    revoked = true;
                    URL.revokeObjectURL(url);
                };
                audio.addEventListener("ended", cleanup, { once: true });
                audio.addEventListener("error", cleanup, { once: true });
                audio.play().catch((err) => {
                    console.error("Failed to play alert sound:", err);
                    cleanup();
                });
            } catch (err) {
                console.error("Failed to parse alert sound data URI:", err);
            }
            return;
        }
    }

    function setActiveAlerts(alerts) {
        const nextAlerts = Array.isArray(alerts) ? alerts.slice() : [];
        const previousAlertKeys = new Set(state.activeAlerts.map(getAlertKey));
        const hasNewAlert = nextAlerts.some((alert) => !previousAlertKeys.has(getAlertKey(alert)));
        const nextSignature = buildAlertSignature(nextAlerts);
        const changed = nextSignature !== state.activeAlertSignature;

        state.activeAlerts = nextAlerts;
        if (changed) {
            state.activeAlertSignature = nextSignature;
            state.activeAlertsChangedAt = Date.now();
            const activeKeys = new Set(nextAlerts.map(getAlertKey));
            state.activeAlertAudioSrcByAlert = new Map(
                Array.from(state.activeAlertAudioSrcByAlert.entries()).filter(([key]) => activeKeys.has(key))
            );
            state.audioProbeStateByRecordingId.clear();
            state.nextAudioAvailabilityCheckAt = nextAlerts.length
                ? state.activeAlertsChangedAt + AUDIO_INITIAL_HOLDOFF_MS
                : 0;
        }

        updateAudioAvailabilityPolling();
        if (hasNewAlert) {
            playNewAlertSound();
        }
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

    function renderStreamCard(card, stream) {
        card.className = `stream-card ${stream.is_connected ? "online" : "offline"}`;
        card.dataset.streamUrl = stream.stream_url;

        const receivingText = stream.is_receiving_audio
            ? "Receiving audio"
            : "No audio activity";
        const statusLabel = stream.is_connected ? "Connected" : "Disconnected";
        const uptime = stream.uptime_seconds
            ? formatDuration(stream.uptime_seconds)
            : "-";

        const lastActivity = stream.last_activity
            ? formatTimestamp(stream.last_activity * 1000)
            : "Never";

        const lastDisconnect = stream.last_disconnect
            ? formatTimestamp(stream.last_disconnect * 1000)
            : "-";

        const connectedSince = stream.connected_since
            ? formatTimestamp(stream.connected_since * 1000)
            : "-";

        const lastAlertReceived = stream.last_alert_received_ts
            ? formatTimestamp(stream.last_alert_received_ts * 1000)
            : "-";

        const streamNickname = window.ICECAST_STREAM_URL_MAPPING?.[stream.stream_url] || "";
        const safeStreamUrl = escapeHtml(stream.stream_url || "");
        const safeLastError = escapeHtml(stream.last_error || "-");
        const safeLastAlertCode = escapeHtml(stream.last_alert_received || "");
        const safeStreamNickname = escapeHtml(streamNickname);
        const streamDisplay = safeStreamNickname
            ? `<strong>${safeStreamNickname}</strong> <span class="smallertext">(<a href="${safeStreamUrl}" target="_blank" rel="noopener noreferrer" style="color: rgba(243, 245, 249, 0.65) !important;">${safeStreamUrl}</a>)</span>`
            : `<a href="${safeStreamUrl}" target="_blank" rel="noopener noreferrer" style="color: rgba(243, 245, 249, 0.65) !important;">${safeStreamUrl}</a>`;

        card.innerHTML = `
            <div class="stream-header">
            <div class="status-tag">${statusLabel}</div>
            <div class="stream-url">${streamDisplay}</div>
            </div>
            <div class="stream-meta">
                <div><strong>Audio:</strong> ${receivingText}</div>
                <div><strong>Uptime:</strong> ${uptime}</div>
                <div><strong>Connected since:</strong> ${connectedSince}</div>
                <div><strong>Last audio:</strong> ${lastActivity}</div>
                <div><strong>Last disconnect:</strong> ${lastDisconnect}</div>
                <div><strong>Attempts:</strong> ${stream.connection_attempts}</div>
                <div><strong>Last error:</strong> ${safeLastError}</div>
                <div><strong>Alerts received:</strong> ${stream.alerts_received}</div>
                <div><strong>Last alert received:</strong> ${safeLastAlertCode ? `${safeLastAlertCode} at ${lastAlertReceived}` : "-"} </div>
            </div>
        `;
    }

    function removeEmptyStreamState() {
        const emptyNode = elements.streamGrid.querySelector(".empty-state");
        if (emptyNode) {
            emptyNode.remove();
        }
    }

    function insertStreamCardSorted(card) {
        const container = elements.streamGrid;
        const cards = container.querySelectorAll("article.stream-card");
        for (const existingCard of cards) {
            const existingUrl = existingCard.dataset.streamUrl || "";
            if (existingUrl.localeCompare(card.dataset.streamUrl || "") > 0) {
                container.insertBefore(card, existingCard);
                return;
            }
        }
        container.appendChild(card);
    }

    function upsertStreamCard(stream) {
        let card = state.streamCardByUrl.get(stream.stream_url);
        if (!card) {
            card = document.createElement("article");
            renderStreamCard(card, stream);
            removeEmptyStreamState();
            insertStreamCardSorted(card);
            state.streamCardByUrl.set(stream.stream_url, card);
            return;
        }
        renderStreamCard(card, stream);
    }

    function scheduleQueuedStreamRender() {
        if (state.streamRenderScheduled) return;
        state.streamRenderScheduled = true;
        const flush = () => {
            state.streamRenderScheduled = false;
            if (!state.pendingStreamUrls.size) return;
            const pendingUrls = Array.from(state.pendingStreamUrls);
            state.pendingStreamUrls.clear();
            pendingUrls.forEach((streamUrl) => {
                const stream = state.streams.get(streamUrl);
                if (stream) {
                    upsertStreamCard(stream);
                }
            });
            elements.streamCount.textContent = `${state.streams.size} tracked`;
        };
        if (typeof window.requestAnimationFrame === "function") {
            window.requestAnimationFrame(flush);
            return;
        }
        setTimeout(flush, STREAM_RENDER_FALLBACK_DELAY_MS);
    }

    function queueStreamRender(streamUrl) {
        if (!streamUrl) return;
        state.pendingStreamUrls.add(streamUrl);
        scheduleQueuedStreamRender();
    }

    function renderStreams() {
        const container = elements.streamGrid;
        const streams = Array.from(state.streams.values()).sort((a, b) =>
            a.stream_url.localeCompare(b.stream_url)
        );
        elements.streamCount.textContent = `${streams.length} tracked`;
        state.pendingStreamUrls.clear();
        state.streamCardByUrl.clear();

        if (!streams.length) {
            container.innerHTML = '<div class="empty-state">No streams configured.</div>';
            return;
        }

        const fragment = document.createDocumentFragment();
        streams.forEach((stream) => {
            const card = document.createElement("article");
            renderStreamCard(card, stream);
            state.streamCardByUrl.set(stream.stream_url, card);
            fragment.appendChild(card);
        });
        container.replaceChildren(fragment);
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
        return `<audio controls preload="none" data-alert-audio="true"><source src="${src}" type="audio/wav">Your browser does not support the audio element.</audio>`;
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
        };

        try {
            const response = await fetch(`/${src}`, {
                method: "HEAD",
                headers,
                cache: "no-store",
            });

            if (!response.ok) return false;

            const contentType = (response.headers.get("content-type") || "").toLowerCase();
            if (
                contentType &&
                (contentType.startsWith("text/") ||
                contentType.includes("html") ||
                contentType.includes("json"))
            ) {
                return false;
            }

            const contentLength = parseInt(response.headers.get("content-length") || "", 10);
            if (Number.isFinite(contentLength) && contentLength <= 0) return false;

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
        if (!state.activeAlerts.length || !hasPendingAlertAudio() || state.audioPollInFlight) {
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
            const availableAudioByAlert = await fetchAvailableAlertAudio(1, 0);
            if (!availableAudioByAlert) return;
            if (state.activeAlertSignature !== alertSignatureAtStart) return;

            let changed = false;
            availableAudioByAlert.forEach((src, key) => {
                if (!src) return;
                if (state.activeAlertAudioSrcByAlert.get(key) !== src) {
                    state.activeAlertAudioSrcByAlert.set(key, src);
                    changed = true;
                }
            });

            if (changed) {
                renderAlerts();
            }
            updateAudioAvailabilityPolling();
        } finally {
            state.audioPollInFlight = false;
        }
    }

    async function precheckAvailableAlertAudio() {
        if (!state.activeAlerts.length || !hasPendingAlertAudio() || state.audioPollInFlight) {
            return;
        }

        state.audioPollInFlight = true;
        try {
            const alertSignatureAtStart = state.activeAlertSignature;
            const availableAudioByAlert = await fetchAvailableAlertAudio(1, 0);
            if (!availableAudioByAlert) return;
            if (state.activeAlertSignature !== alertSignatureAtStart) return;

            let changed = false;
            availableAudioByAlert.forEach((src, key) => {
                if (!src) return;
                if (state.activeAlertAudioSrcByAlert.get(key) !== src) {
                    state.activeAlertAudioSrcByAlert.set(key, src);
                    changed = true;
                }
            });

            if (changed) {
                renderAlerts();
            }
            updateAudioAvailabilityPolling();
        } finally {
            state.audioPollInFlight = false;
        }
    }

    function bindAudioUnavailableFallback(card, alertKey) {
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
            if (
                alertKey &&
                failedSrc &&
                state.activeAlertAudioSrcByAlert.get(alertKey) === failedSrc
            ) {
                state.activeAlertAudioSrcByAlert.delete(alertKey);
                updateAudioAvailabilityPolling();
            }
        };

        audioEl.addEventListener("error", showUnavailable, { once: true });
        if (sourceEl) {
            sourceEl.addEventListener("error", showUnavailable, { once: true });
        }
    }

    async function fetchAvailableAlertAudio(maxAttempts = 4, delayMs = 600) {
        const alerts = getSortedActiveAlerts();
        if (!alerts.length) {
            return new Map();
        }

        for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
            const latestRecordingId = await fetchAudioUrl();
            if (latestRecordingId !== null) {
                pruneProbeState(latestRecordingId, alerts.length);
                const now = Date.now();
                const entries = [];
                const probeTargets = [];

                alerts.forEach((alert, index) => {
                    const key = getAlertKey(alert);
                    const existingSrc = state.activeAlertAudioSrcByAlert.get(key);
                    if (existingSrc) {
                        entries.push([key, existingSrc]);
                        return;
                    }

                    const recordingId = latestRecordingId - index;
                    if (recordingId < 0) {
                        entries.push([key, null]);
                        return;
                    }

                    if (shouldProbeRecordingId(recordingId, now)) {
                        probeTargets.push({ key, recordingId });
                    }
                });

                const limitedTargets = probeTargets.slice(0, AUDIO_PROBE_BATCH_SIZE);
                const probedEntries = await mapWithConcurrency(
                    limitedTargets,
                    AUDIO_PROBE_CONCURRENCY,
                    async ({ key, recordingId }) => {
                        const src = `archive.php?recording_id=${recordingId}`;
                        const available = await isAudioAvailable(src);
                        if (available) {
                            markRecordingIdProbeSuccess(recordingId);
                            return [key, src];
                        }
                        markRecordingIdProbeFailure(recordingId);
                        return [key, null];
                    }
                );

                entries.push(...probedEntries);

                const available = new Map(entries);
                if (Array.from(available.values()).some(Boolean)) {
                    return available;
                }
            }

            if (attempt < maxAttempts - 1) {
                await new Promise((resolve) => setTimeout(resolve, delayMs));
            }
        }

        return null;
    }

    async function loadSameUsLookups() {
        if (state.sameUsByFips && state.sameUsSubdivByCode) return;
        if (state.sameUsLoadPromise) {
            await state.sameUsLoadPromise;
            return;
        }

        state.sameUsLoadPromise = (async () => {
            try {
                const payload = await fetchJson("/api/same-us");
                if (!payload) {
                    throw new Error("Empty /api/same-us response");
                }
                const same = payload?.SAME;
                const subdiv = payload?.SUBDIV;

                state.sameUsByFips = same && typeof same === "object" ? same : {};
                state.sameUsSubdivByCode = subdiv && typeof subdiv === "object" ? subdiv : {};
                state.locationLabelCache.clear();
            } catch (err) {
                console.error("Failed to load SAME lookup table", err);
                state.sameUsByFips = {};
                state.sameUsSubdivByCode = {};
                state.locationLabelCache.clear();
            } finally {
                state.sameUsLoadPromise = null;
                if (state.activeAlerts.length) {
                    renderAlerts();
                }
            }
        })();

        await state.sameUsLoadPromise;
    }

    function formatLocationLabel(locationCode) {
        if (state.locationLabelCache.has(locationCode)) {
            return state.locationLabelCache.get(locationCode);
        }

        const subdivCode = locationCode.charAt(0);
        const sameCode = locationCode.slice(1);
        const sameName = state.sameUsByFips?.[sameCode];

        if (!sameName) {
            state.locationLabelCache.set(locationCode, locationCode);
            return locationCode;
        }

        const subdivisionName = state.sameUsSubdivByCode?.[subdivCode] || "";
        const withSubdivision = subdivisionName ? `${subdivisionName} ${sameName}` : sameName;
        const withoutCounty = withSubdivision
            .replace(LOCATION_COUNTY_PATTERN, "")
            .replace(/\s{2,}/g, " ")
            .replace(/\s+,/g, ",")
            .trim();
        const expanded = expandStateAbbreviations(withoutCounty);

        state.locationLabelCache.set(locationCode, expanded);
        return expanded;
    }

    function locationParser(locations) {
        if (!locations) return "";
        if (!state.sameUsByFips || !state.sameUsSubdivByCode) {
            void loadSameUsLookups();
        }

        const locationCodes = String(locations).match(LOCATION_CODE_PATTERN);
        if (!locationCodes || !locationCodes.length) {
            const expanded = expandStateAbbreviations(String(locations).trim());
            return normalizeLocationSeparators(expanded);
        }

        return locationCodes.map((locationCode) => formatLocationLabel(locationCode)).join("; ");
    }

    function renderAlerts() {
        const container = elements.alertList;
        container.innerHTML = "";
        const alerts = getSortedActiveAlerts();
        elements.alertCount.textContent = alerts.length ? `${alerts.length} active` : "None";

        if (!alerts.length) {
            container.innerHTML = '<div class="empty-state">No active alerts.</div>';
            return;
        }

        alerts.forEach((alert) => {
            const card = document.createElement("article");
            const normalizedEventText = String(alert?.data?.event_text || "").replace(/^(?:a|an|the)\s+/i, "").trim();
            const parsedEventText = /has issued(?: an?| the)? (.*?) for/i.exec(alert?.data?.eas_text || "");
            const eventText = normalizedEventText || parsedEventText?.[1] || "No headline available";
            const severity = RegExp(/(warning|watch|advisory|emergency|test|alert|message)/i).exec(eventText)?.[1]?.toLowerCase();
            const alertKey = getAlertKey(alert);
            const availableAudioSrc = state.activeAlertAudioSrcByAlert.get(alertKey) || "";
            const eventCode = eventCodeForAlert(alert) || "—";
            const originator = originatorForAlert(alert) || "—";
            const locationCodes = locationCodesForAlert(alert);

            card.className = `alert-card ${severity || "unknown"}`;
            card.innerHTML = `
                <div class="event-code">${eventCode}</div>
                <div class="headline">${eventText}</div>
                <div class="meta">
                    <div>${alert.data.eas_text || "Alert received."}</div>
                    <br>
                    <div><strong>Originator:</strong> ${originator}</div>
                    <br>
                    <div><strong>Severity:</strong> ${severity ? severity.toUpperCase() : "Unknown"}</div>
                    <br>
                    <div><strong>Locations:</strong> ${locationParser(locationCodes) || "—"}</div>
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
            bindAudioUnavailableFallback(card, alertKey);
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

    function renderCapStatus() {
        const section = elements.capStatusSection;
        const container = elements.capStatus;
        if (!container) return;
        container.innerHTML = "";

        const cap = state.capStatus;
        if (!cap || typeof cap !== "object") {
            if (section) section.style.display = "";
            container.innerHTML = '<div class="empty-state">CAP status unavailable.</div>';
            return;
        }

        if (!cap.enabled) {
            if (section) section.style.display = "none";
            return;
        }

        if (section) section.style.display = "";

        const endpointCount = Number(cap.endpoint_count) || 0;
        const endpoints = Array.isArray(cap.endpoints) ? cap.endpoints : [];
        const endpointRows = endpoints.length
            ? endpoints
                .map((entry, index) => {
                    const endpointUrl = typeof entry === "string" ? entry : (entry?.url || "");
                    if (!endpointUrl) {
                        return `<div class="cap-endpoint-row"><strong>Endpoint ${index + 1}</strong></div>`;
                    }

                    let endpointName =
                        typeof entry === "object" && typeof entry?.name === "string"
                            ? entry.name.trim()
                            : "";
                    if (!endpointName) {
                        try {
                            endpointName = new URL(endpointUrl).hostname;
                        } catch (_err) {
                            endpointName = `Endpoint ${index + 1}`;
                        }
                    }

                    const safeName = escapeHtml(endpointName);
                    const safeUrl = escapeHtml(endpointUrl);
                    return `<div class="cap-endpoint-row"><strong>${safeName}</strong> <span class="smallertext">(<a href="${safeUrl}" target="_blank" rel="noopener noreferrer" style="color: rgba(243, 245, 249, 0.65) !important;">${safeUrl}</a>)</span></div>`;
                })
                .join("")
            : '<div class="cap-endpoint-row">None configured</div>';
        const lastPoll = cap.last_poll_at ? formatTimestamp(cap.last_poll_at * 1000) : "Never";
        const lastGoodPoll = cap.last_successful_poll_at
            ? formatTimestamp(cap.last_successful_poll_at * 1000)
            : "Never";
        const lastAlertAt = cap.last_alert_received_at
            ? formatTimestamp(cap.last_alert_received_at * 1000)
            : "—";
        const lastAlertCode = cap.last_alert_event_code
            ? escapeHtml(cap.last_alert_event_code)
            : "—";
        const lastAlertSource = cap.last_alert_source
            ? escapeHtml(cap.last_alert_source)
            : "—";
        const pollError = cap.last_poll_error ? escapeHtml(cap.last_poll_error) : "";

        const card = document.createElement("article");
        card.className = `cap-card ${pollError ? "degraded" : "healthy"}`;
        card.innerHTML = `
            <div class="cap-header">
                <div class="status-tag">${pollError ? "Degraded" : "Healthy"}</div>
                <div class="cap-subtitle">CAP monitor is active and publishing status updates.</div>
            </div>
            <div class="cap-meta">
                <span><strong>Status:</strong> ${cap.enabled ? "Enabled" : "Disabled"}</span>
                <span><strong>Endpoints:</strong> ${endpointCount}</span>
                <span><strong>Poll attempts:</strong> ${cap.polls_attempted || 0}</span>
                <span><strong>Poll failures:</strong> ${cap.polls_failed || 0}</span>
                <span><strong>Processed CAP alerts:</strong> ${cap.alerts_processed || 0}</span>
                <span><strong>Active CAP alerts:</strong> ${cap.active_alerts || 0}</span>
                <span><strong>Last poll:</strong> ${lastPoll}</span>
                <span><strong>Last successful poll:</strong> ${lastGoodPoll}</span>
                <span><strong>Last CAP alert:</strong> ${lastAlertCode} at ${lastAlertAt}</span>
                <span><strong>Last CAP source:</strong> ${lastAlertSource}</span>
            </div>
            ${pollError ? `<div class="cap-error"><strong>Last poll error:</strong><pre>${pollError}</pre></div>` : ""}
            <div class="cap-endpoints">
                <div class="cap-endpoints-title">Configured endpoints</div>
                ${endpointRows}
            </div>
        `;
        container.appendChild(card);
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
            loadSameUsLookups(),
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
                        queueStreamRender(payload.payload.stream_url);
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
                case "CapStatus":
                    if (payload.payload && typeof payload.payload === "object") {
                        state.capStatus = payload.payload;
                        renderCapStatus();
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
