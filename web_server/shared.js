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

        return `<audio ${attrs.join(" ")}><source src="${src}" type="audio/wav">Your browser does not support the audio element.</audio>`;
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

    const shared = Object.assign(window.shared || {}, {
        formatTimestamp,
        fetchAudioMarkup: fetch_audio,
        downloadAudio,
    });

    window.shared = shared;
    window.formatTimestamp = formatTimestamp;
    window.fetch_audio = fetch_audio;
    window.downloadAudio = downloadAudio;
})();
