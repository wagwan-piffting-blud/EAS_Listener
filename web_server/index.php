<?php

function handle_redirect($current_url, $redirect_url = null)
{
    if (!empty($redirect_url) && isset($redirect_url)) {
        if ($current_url !== $redirect_url) {
            unset($_SESSION["redirect"]);
            header("Location: " . basename($redirect_url));
            exit();
        }
    }
}

if (!session_id()) {
    if (getenv('USE_REVERSE_PROXY') === 'true') {
        session_set_cookie_params(259200, "/", "", true, true);
    } else {
        session_set_cookie_params(259200, "/", "", false, true);
    }

    session_start();
}

if (!empty($_POST["username"]) && !empty($_POST["password"])) {
    $valid_user = getenv('DASHBOARD_USERNAME');
    $valid_pass = getenv('DASHBOARD_PASSWORD');

    if ($_POST["username"] === $valid_user && $_POST["password"] === $valid_pass) {
        $_SESSION['authed'] = true;
    } else {
        echo "<script>alert('Invalid username or password.'); window.location='" . basename($_SERVER["SCRIPT_FILENAME"]) . "';</script>";
        exit();
    }
}

if (!isset($_SESSION['authed'])) {
    $_SESSION["redirect"] = $_GET["redirect"] ?? null; ?><!DOCTYPE html>
    <html lang="en">

    <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <title>EAS Dashboard - Login</title>
        <style>
            :root {
                color-scheme: dark light;
                --bg: #11161f;
                --fg: #f3f5f9;
                --panel: #1c2330;
                --accent: #4f9dff;
                --accent-soft: rgba(79, 157, 255, 0.2);
                --success: #3dd068;
                --warn: #ffb347;
                --error: #ff6b6b;
                --muted: rgba(243, 245, 249, 0.65);
                --border: rgba(243, 245, 249, 0.12);
                font-family: "Segoe UI", "Inter", "Helvetica Neue", system-ui, -apple-system, sans-serif;
                background: radial-gradient(circle at top, rgba(79, 157, 255, 0.12), transparent) no-repeat,
                    var(--bg);
                color: var(--fg);
            }

            *,
            *::before,
            *::after {
                box-sizing: border-box;
            }

            body {
                margin: 0;
                min-height: 100vh;
                display: flex;
                flex-direction: column;
                overflow-x: hidden;
            }

            .container {
                display: flex;
                flex-direction: column;
                justify-content: center;
                align-items: center;
                height: 100vh;
                text-align: center;
                padding: 1rem;
            }

            h1 {
                font-size: 2rem;
                margin-bottom: 1rem;
            }

            input {
                font-size: 24px;
                color: #bbb;
            }
        </style>
    </head>

    <body>
        <div class="container">
            <h1>Please login to view the EAS Monitoring Dashboard.</h1>
            <form method="POST" action="<?php print_r(basename($_SERVER["SCRIPT_FILENAME"])); ?>">
                <input type="text" name="username" placeholder="Username" required /><br /><br />
                <input type="password" name="password" placeholder="Password" required /><br /><br />
                <button type="submit">Login</button>
            </form>
        </div>
    </body>
</html><?php } else {
    handle_redirect($_SERVER["REQUEST_URI"], $_SESSION['redirect'] ?? null); ?><!DOCTYPE html>
<html lang="en">
    <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <title>EAS Monitoring Dashboard</title>
        <link rel="stylesheet" href="/style.css" />
    </head>
    <body>
        <header>
            <h1><img src="assets/favicon-96x96.png" alt="EAS Logo" class="logo" />EAS Monitoring Dashboard</h1>
            <div id="header-right">
                <span id="wsStatus" class="ws-status">Connecting...</span>
                <div id="logout-container">
                    <a class="custom-button" href="reload.php" class="button">Reload Rust Backend</a>
                    <a class="custom-button" href="logout.php" class="button">Logout</a>
                </div>
            </div>
        </header>
        <main>
            <section id="streamSection">
                <h2>
                    Streams
                    <span id="streamCount" class="pill">0 tracked</span>
                </h2>
                <div id="streamGrid" class="stream-grid section-scroll"></div>
            </section>
            <section id="alertSection">
                <h2>
                    Active Alerts
                    <span id="alertCount" class="pill">None</span>
                </h2>
                <p class="smalltext" id="activetext">Alerts are shown in reverse chronological order (newest to oldest, top to bottom) here. <a href="archive.php">Click here to view all archived alerts.</a></p>
                <div id="alertList" class="section-scroll"></div>
            </section>
            <section id="logSection">
                <h2>
                    Recent Logs
                    <span id="logCount" class="pill">0 entries</span>
                </h2>
                <div id="logList" class="logs-container section-scroll"></div>
            </section>
        </main>
        <footer>
            <span>Powered by <a id="updateLink" data-text="Wags' Rust EAS Listener" href="https://github.com/wagwan-piffting-blud/EAS_Listener" target="_blank">Wags' Rust EAS Listener</a> | <a href="/chargen.php">Enter Character Generator mode</a> | <a href="/vacuum.php">Vacuum old alerts</a></span>
        </footer>
        <script>
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

            const localVersion = <?php
                $cargoToml = file_get_contents("/app/Cargo.toml");
                preg_match('/^\s*\[package\]\s*$([\s\S]*?)(^\s*\[|\s*\Z)/m', $cargoToml, $pkgMatch);
                if ($pkgMatch) {
                    $pkgBody = $pkgMatch[1];
                    preg_match('/^\s*version\s*=\s*["\']([^"\']+)["\']\s*(?:#.*)?$/m', $pkgBody, $verMatch);
                    if ($verMatch) {
                        echo json_encode(trim($verMatch[1]));
                    } else {
                        echo json_encode("unknown");
                    }
                } else {
                    echo json_encode("unknown");
                }
            ?>;

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

            window.API_BASE = "<?php if (getenv('USE_REVERSE_PROXY') == 'true') {
                print_r(getenv('WS_REVERSE_PROXY_URL'));
            } else {
                print_r(substr($_SERVER['HTTP_HOST'], 0, strpos($_SERVER['HTTP_HOST'], ':') ?: strlen($_SERVER['HTTP_HOST'])) . ":" . getenv('MONITORING_BIND_PORT') ?: '8080');
            }
            ?>";

            window.TOKEN = "<?php print_r(base64_encode(getenv('DASHBOARD_USERNAME') . ':' . getenv('DASHBOARD_PASSWORD'))); ?>";

            window.MONITORING_MAX_LOGS = <?php echo getenv('MONITORING_MAX_LOGS') ?: '500'; ?>;

            window.ALERTSOUNDDATA = "<?php include 'alert_noise.php'; ?>";

            window.ALERTSOUNDENABLED = <?php echo (getenv('ALERT_SOUND_ENABLED') === 'true') ? 'true' : 'false'; ?>;
        </script>
        <script src="index.js"></script>
    </body>
</html><?php } ?>
