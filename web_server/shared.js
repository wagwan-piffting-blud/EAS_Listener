(function () {
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

    function formatTimestamp(ts, withTime = true) {
        if (ts === null || ts === undefined) return "-";
        const date = new Date(ts);
        if (Number.isNaN(date.getTime())) return "-";
        return (withTime ? TIMESTAMP_WITH_TIME_FORMATTER : TIMESTAMP_DATE_ONLY_FORMATTER).format(date);
    }

    function fetch_audio(src, options = {}) {
        if (!src) return options.unavailableMarkup ?? false;

        const attrs = [];
        if (options.controls !== false) attrs.push("controls");
        if (options.preload) attrs.push(`preload="${options.preload}"`);
        if (options.dataAlertAudio) attrs.push('data-alert-audio="true"');

        return `<audio ${attrs.join(" ")}><source src="${src}">Your browser does not support the audio element.</audio>`;
    }

    function downloadAudio(src) {
        if (!src) return;
        const link = document.createElement("a");
        link.href = src;
        link.download = src.split("/").pop()?.split("?")[0] || "alert_audio.wav";
        document.body.appendChild(link);
        link.click();
        document.body.removeChild(link);
    }

    function apiUrl(path) {
        const protocol = window.location.protocol === "https:" ? "https" : "http";
        return `${protocol}://${window.API_BASE}${path}`;
    }

    function apiFetch(path, options = {}) {
        const headers = Object.assign({}, options.headers);
        if (window.TOKEN) headers.Authorization = `Bearer ${window.TOKEN}`;
        return fetch(apiUrl(path), Object.assign({}, options, {
            headers,
            credentials: "same-origin",
        }));
    }

    // An <audio> element cannot send an Authorization header. Same-origin requests carry the
    // session cookie automatically, so a token is only appended when one is explicitly in play.
    function apiRecordingUrl(params) {
        const query = new URLSearchParams(params);
        if (window.TOKEN) query.set("auth", window.TOKEN);
        return apiUrl(`/api/recordings?${query.toString()}`);
    }

    const shared = Object.assign(window.shared || {}, {
        formatTimestamp,
        fetchAudioMarkup: fetch_audio,
        downloadAudio,
        apiUrl,
        apiFetch,
        apiRecordingUrl,
    });

    window.shared = shared;
    window.formatTimestamp = formatTimestamp;
    window.fetch_audio = fetch_audio;
    window.downloadAudio = downloadAudio;
    window.apiUrl = apiUrl;
    window.apiFetch = apiFetch;
    window.apiRecordingUrl = apiRecordingUrl;
})();
