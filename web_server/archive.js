const alertList = document.getElementById("oldAlertList");
const alertCount = document.getElementById("oldAlertCount");
const filterStatus = document.getElementById("filterStatus");
const filterOptions = document.getElementById("filterOptions");
const fipsFilterToggle = document.getElementById("fipsFilterToggle");
const WATCHED_FIPS_FILTER_DEFAULT = true;
let filterWatchedFips = WATCHED_FIPS_FILTER_DEFAULT;

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

async function fetchArchivedAlerts() {
    const params = new URLSearchParams({ fetch_alerts: "true" });
    if (filterWatchedFips) {
        params.set("filter_alerts", "watched_fips");
    }

    return fetch(`archive.php?${params.toString()}`)
        .then((response) => response.json())
        .catch(() => []);
}

function fetch_audio(src) {
    if (!src) return false;
    return `<audio controls><source src="${src}" type="audio/wav">Your browser does not support the audio element.</audio>`;
}

async function renderAlerts() {
    const container = alertList;
    container.innerHTML = "";
    const alerts = await fetchArchivedAlerts();
    alertCount.textContent = alerts.length ? `${alerts.length} received overall` : "None";

    if (!alerts.length) {
        container.innerHTML = '<div class="empty-state">No archived alerts.</div>';
        return;
    }

    for (const alert of alerts) {
        const card = document.createElement("article");
        const severityClass = alert?.data?.alert_severity ? alert.data.alert_severity.toLowerCase() : "unknown";
        const recordingMarkup = filterWatchedFips && alert.data.audio_recording
            ? `
                <br>
                <div><strong>Recording audio:&ensp;</strong> ${fetch_audio(alert.data.audio_recording)}</div>
            `
            : "";

        card.className = `alert-card ${severityClass}`;
        const parsedEventText = /has issued(?: an?| the)? (.*?) for/i.exec(alert?.data?.eas_text || "");
        const eventText = alert?.data?.event_text || parsedEventText?.[1] || "No headline available";
        card.innerHTML = `
            <div class="event-code">${alert.data.event_code}</div>
            <div class="headline">${eventText}</div>
            <div class="meta">
                <div>${alert.data.eas_text || "Alert received."}</div>
                <br>
                <div><strong>Originator:</strong> ${alert.data.originator}</div>
                <br>
                <div><strong>Severity:</strong> ${alert.data.alert_severity ? alert.data.alert_severity.toUpperCase() : "Unknown"}</div>
                <br>
                <div><strong>Locations:</strong> ${alert.data.locations || "—"}</div>
                <br>
                <div><strong>Received:</strong> ${formatTimestamp(alert.received_at * 1000)}</div>
                <br>
                <div><strong>Expired:</strong> ${formatTimestamp(alert.expired_at * 1000)}</div>
                <br>
                <div><strong>Length:</strong> ${alert.data.length ? `${Math.floor(alert.data.length / 100)}h ${alert.data.length % 100}m` : "—"}</div>
                <br>
                <div><strong>Raw ZCZC String:</strong> <pre>${alert.data.raw_zczc || "—"}</pre></div>
                ${recordingMarkup}
            </div>
        `;
        container.appendChild(card);
    }

    window.alertListBeforeFiltering = alertList.querySelectorAll(".alert-card");
}

function filterDialog() {
    const options = [
        { label: "Emergency Alerts Only", value: "emergency" },
        { label: "Warning Alerts Only", value: "warning" },
        { label: "Watch Alerts Only", value: "watch" },
        { label: "Advisory Alerts Only", value: "advisory" },
        { label: "Test Alerts Only", value: "test" },
    ];

    const dialog = document.createElement("dialog");
    dialog.className = "filter-dialog";
    dialog.innerHTML = `
        <form method="dialog">
            <h3>Filter Archived Alerts</h3>
            <label for="searchInput" class="muted">Filter by Event Code:</label>
            <br>
            <input id="searchInput" name="searchInput" type="text" placeholder="Event Code" pattern="[A-Z]{3}" title="Three letter event code (e.g. RWT, NPT, etc.)" maxlength="3" autocomplete="off">
            <br>
            <br>
            <label class="muted">Or filter by Alert Severity:</label>
            <br>
            ${options.map(opt => `
                <div>
                    <input type="radio" id="filter-${opt.value}" name="filter" value="${opt.value}">
                    <label for="filter-${opt.value}">${opt.label}</label>
                </div>
            `).join('')}
            <br>
            <div class="dialog-actions">
                <button type="submit">Apply</button>
                <button type="reset" id="resetBtn">Reset</button>
                <button type="button" id="cancelBtn">Cancel</button>
            </div>
        </form>
    `;

    document.body.appendChild(dialog);

    const searchInput = dialog.querySelector("#searchInput");
    searchInput.addEventListener("keyup", (e) => {
        if (e.key === "Enter") {
            e.preventDefault();
            dialog.querySelector("form").requestSubmit();
        }
        if (e.key.length === 1 && e.key >= 'a' && e.key <= 'z') {
            e.target.value = e.target.value.toUpperCase();
        }
        if (e.key.length !== 1 && !["Backspace", "Delete", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(e.key)) {
            e.preventDefault();
        }
    });

    dialog.querySelectorAll('input[name="filter"]').forEach(radio => {
        radio.onchange = () => {
            if (radio.checked) {
                searchInput.value = "";
                currentFilter = radio.value;
            }
        };
    });

    searchInput.value = currentFilter.length === 3 ? currentFilter : "";
    searchInput.oninput = () => {
        const value = searchInput.value.toUpperCase();
        if (/^[A-Z]{3}$/.test(value)) {
            currentFilter = value;
            dialog.querySelectorAll('input[name="filter"]').forEach(radio => radio.removeAttribute("checked"));
        } else if (value === "") {
            currentFilter = "all";
            dialog.querySelector(`#filter-all`).setAttribute("checked", "true");
        } else {
            currentFilter = "";
            dialog.querySelectorAll('input[name="filter"]').forEach(radio => radio.removeAttribute("checked"));
        }
    };

    if (currentFilter.length === 3) {
        searchInput.setAttribute("checked", "true");
    } else if (options.some(opt => opt.value === currentFilter)) {
        searchInput.value = "";
    } else {
        currentFilter = "all";
    }

    const resetBtn = dialog.querySelector("#resetBtn");
    resetBtn.addEventListener("click", () => {
        searchInput.value = "";
        currentFilter = "ALL";
        dialog.querySelectorAll('input[name="filter"]').forEach(radio => radio.removeAttribute("checked"));
        filterStatus.textContent = "Showing All";
        applyFilter();
        dialog.close();
        dialog.remove();
    });

    dialog.querySelector(`#filter-${currentFilter}`)?.setAttribute("checked", "true");

    dialog.querySelector("form").onsubmit = (e) => {
        e.preventDefault();
        const selected = dialog.querySelector('input[name="filter"]:checked');
        const searchValue = searchInput.value.toUpperCase();
        if (/^[A-Z]{3}$/.test(searchValue)) {
            currentFilter = searchValue;
            filterStatus.textContent = `Event Code: ${searchValue}`;
        } else if (selected) {
            currentFilter = selected.value;
            filterStatus.textContent = selected.parentElement.textContent.trim();
        }
        applyFilter();
        dialog.close();
        dialog.remove();
    };

    dialog.querySelector("#cancelBtn").addEventListener("click", () => {
        dialog.close();
        dialog.remove();
    });

    dialog.showModal();
}

function resetCardVisibility() {
    const alertList = document.getElementById("oldAlertList");
    if (alertList.querySelector(".empty-state")) {
        alertList.querySelector(".empty-state").remove();
    }
    window.alertListBeforeFiltering?.forEach(card => {
        card.style.display = "";
        alertList.appendChild(card);
    });
}

function applyFilter() {
    const cards = alertList.getElementsByClassName("alert-card");
    const isEventCodeFilter = currentFilter.length === 3;
    let visibleCount = 0;

    resetCardVisibility();

    for (const card of cards) {
        const severity = card.className.split(" ").pop();
        const eventCode = card.querySelector(".event-code").textContent;

        if (currentFilter === "ALL" || (["emergency", "warning", "watch", "advisory", "test"].includes(currentFilter) && severity === currentFilter) || (isEventCodeFilter && eventCode === currentFilter)) {
            card.style.display = "";
            visibleCount++;
        } else {
            card.style.display = "none";
        }
    }

    alertCount.textContent = visibleCount ? `${visibleCount} alert(s) shown` : "No alerts match the filter";
    if (visibleCount === 0) {
        alertList.innerHTML = '<div class="empty-state">No alerts match the filter.</div>';
    } else if (alertList.querySelector(".empty-state")) {
        alertList.querySelector(".empty-state").remove();
    }
}

let currentFilter = "ALL";
filterOptions.onclick = filterDialog;

function updateFipsFilterToggle() {
    if (!fipsFilterToggle) return;
    const label = filterWatchedFips ? "Showing Watched FIPS" : "Showing All Alerts";
    fipsFilterToggle.textContent = label;
    fipsFilterToggle.setAttribute("aria-pressed", filterWatchedFips ? "true" : "false");
}

function toggleFipsFilter() {
    filterWatchedFips = !filterWatchedFips;
    updateFipsFilterToggle();
    renderAlerts();
}

if (fipsFilterToggle) {
    fipsFilterToggle.addEventListener("click", toggleFipsFilter);
    fipsFilterToggle.addEventListener("keydown", (event) => {
        if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            toggleFipsFilter();
        }
    });
    updateFipsFilterToggle();
}

renderAlerts();
