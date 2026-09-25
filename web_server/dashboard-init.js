async function fetchGitHubCargoVersion({owner, repo, branch = "main", path = "Cargo.toml", timeoutMs = 8000}) {
    const url = `https://raw.githubusercontent.com/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/${encodeURIComponent(branch)}/${path
    .split("/")
    .map(encodeURIComponent)
    .join("/")}`;

    const controller = new AbortController();
    const t = setTimeout(() => controller.abort(), timeoutMs);

    try {
        const res = await fetch(url, {
            signal: controller.signal,
            cache: "no-store",
            headers: {
                "Accept": "text/plain",
            },
        });
        if (!res.ok) {
            throw new Error(`GitHub raw fetch failed: ${res.status} ${res.statusText}`);
        }
        const toml = await res.text();
        const version = parseCargoTomlPackageVersion(toml);
        if (!version) throw new Error("Could not find [package] version in Cargo.toml");
        return version;
    } finally {
        clearTimeout(t);
    }
}

function parseCargoTomlPackageVersion(tomlText) {
    const pkgMatch = tomlText.match(/^\s*\[package\]\s*$([\s\S]*?)(^\s*\[|\s*\Z)/m);
    if (!pkgMatch) return null;

    const pkgBody = pkgMatch[1];

    const verMatch = pkgBody.match(/^\s*version\s*=\s*["']([^"']+)["']\s*(?:#.*)?$/m);
    return verMatch ? verMatch[1].trim() : null;
}

function compareSemver(a, b) {
    const A = parseSemver(a);
    const B = parseSemver(b);

    if (!A || !B) return a === b ? 0 : (a < b ? -1 : 1);

    for (const k of ["major", "minor", "patch"]) {
        if (A[k] !== B[k]) return A[k] < B[k] ? -1 : 1;
    }

    if (A.prerelease && !B.prerelease) return -1;
    if (!A.prerelease && B.prerelease) return 1;

    return 0;
}

function parseSemver(v) {
    const m = String(v).trim().match(
        /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?$/
    );
    if (!m) return null;
    return {
        major: Number(m[1]),
        minor: Number(m[2]),
        patch: Number(m[3]),
        prerelease: m[4] || "",
        build: m[5] || "",
    };
}

function isNewerVersionAvailable(localVersion, remoteVersion) {
    return compareSemver(localVersion, remoteVersion) < 0;
}

const localVersion = window.APP_VERSION;

document.getElementById("currentVersion").textContent = localVersion;

(async () => {
    const remoteVersion = await fetchGitHubCargoVersion({
        owner: "wagwan-piffting-blud",
        repo: "EAS_Listener",
        branch: "main",
        path: "Cargo.toml",
    });

    if (isNewerVersionAvailable(localVersion, remoteVersion)) {
        const dismissKey = `dismiss_update_${remoteVersion}`;
        if (!localStorage.getItem(dismissKey)) {
            alert(`A new version of EAS_Listener is available: ${remoteVersion}! (You are currently on version ${localVersion}.) See the EAS_Listener GitHub Wiki for update instructions for your version.`);
            localStorage.setItem(dismissKey, "1");
        }
        document.getElementById("updateLink")?.classList.add("pulse");
        document.getElementById("updateLink").innerHTML += ` (Update Available: v${remoteVersion})`;
        document.getElementById("updateLink").dataset.text += ` (Update Available: v${remoteVersion})`;
    }
})().catch((err) => {
    console.warn("Update check failed:", err);
});

async function postAction(path, button, busyText) {
    const original = button ? button.textContent : "";
    if (button) {
        button.disabled = true;
        button.textContent = busyText;
    }
    try {
        const response = await window.apiFetch(path, { method: "POST" });
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return await response.json();
    } catch (err) {
        console.error(`${path} failed:`, err);
        alert(`That did not work: ${err.message}`);
        return null;
    } finally {
        if (button) {
            button.disabled = false;
            button.textContent = original;
        }
    }
}

document.getElementById("reloadButton").addEventListener("click", async (event) => {
    if (!confirm("Are you sure you want to reload the configuration and Rust backend? This will temporarily interrupt monitoring while the backend restarts.")) return;
    const result = await postAction("/api/reload", event.currentTarget, "Reloading...");
    if (result) alert("Reload signal sent. The backend will pick up the new configuration within a second or two.");
});

document.getElementById("testAlertButton").addEventListener("click", async (event) => {
    if (!confirm("Send a synthetic Required Weekly Test (RWT) through the FULL alert pipeline? This decodes, logs, records, sends notifications (Apprise/Discord), and runs any configured relays exactly as a real alert would. Its recording is narrated by the configured TTS engine, so this also checks that engine works. It will appear on the dashboard and in the archive.")) return;
    const result = await postAction("/api/test-alert", event.currentTarget, "Sending...");
    if (result) alert("Test alert injected. Watch the dashboard, your notifications and the archive to confirm each stage.\n\nThe log reports \"Test alert TTS OK\" or \"Test alert TTS FAILED\" once the engine has run; play the recording to hear the voice.");
});

document.getElementById("vacuumLink").addEventListener("click", async (event) => {
    event.preventDefault();
    if (!confirm("Vacuum old recordings and truncate the alert log?\n\nRecordings that are not currently active move to an __old__ subdirectory and the alert log is cleared of non-active alerts. Nothing is deleted: recordings are moved and the log is backed up first.")) return;
    if (!confirm("Are you absolutely sure? This cannot be undone automatically.")) return;

    const link = event.currentTarget;
    const original = link.textContent;
    link.textContent = "Vacuuming...";
    try {
        const response = await window.apiFetch("/api/vacuum", { method: "POST" });
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        const report = await response.json();
        alert(
            `Vacuum complete.

` +
            `Alerts removed: ${report.alerts_deleted}
` +
            `Recordings moved to __old__: ${report.recordings_archived}
` +
            `Recordings kept: ${report.recordings_kept}
` +
            `Alert log entries retained: ${report.log_entries_retained}` +
            (report.log_backed_up ? ` (backed up to .bak)` : ``)
        );
        window.location.reload();
    } catch (err) {
        console.error("Vacuum failed:", err);
        alert(`Vacuum failed: ${err.message}`);
    } finally {
        link.textContent = original;
    }
});
        
