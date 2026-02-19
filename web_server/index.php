<?php

function handle_redirect($current_url, $redirect_url = null) {
    if(!empty($redirect_url) && isset($redirect_url)) {
        if($current_url !== $redirect_url) {
            unset($_SESSION["redirect"]);
            header("Location: " . basename($redirect_url));
            exit();
        }
    }
}

if(!session_id()) {
    if(getenv('USE_REVERSE_PROXY') === 'true') {
        session_set_cookie_params(259200, "/", "", true, true);
    }

    else {
        session_set_cookie_params(259200, "/", "", false, true);
    }

    session_start();
}

if(!empty($_POST["username"]) && !empty($_POST["password"])) {
    $valid_user = getenv('DASHBOARD_USERNAME');
    $valid_pass = getenv('DASHBOARD_PASSWORD');

    if($_POST["username"] === $valid_user && $_POST["password"] === $valid_pass) {
        $_SESSION['authed'] = true;
    }

    else {
        echo "<script>alert('Invalid username or password.'); window.location='" . basename($_SERVER["SCRIPT_FILENAME"]) . "';</script>";
        exit();
    }
}

if(!isset($_SESSION['authed'])) { $_SESSION["redirect"] = $_GET["redirect"] ?? null; ?><!DOCTYPE html>
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
</html><?php } else { handle_redirect($_SERVER["REQUEST_URI"], $_SESSION['redirect'] ?? null); ?><!DOCTYPE html>
<html lang="en">
    <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <title>EAS Monitoring Dashboard</title>
        <link rel="stylesheet" href="style.css" />
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
                <a class="smalltext" href="archive.php">(click here to view all archived alerts)</a>
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
            Data provided by the container's monitoring backend. Updates in real time.
        </footer>
        <script>
            window.API_BASE = "<?php if(getenv('USE_REVERSE_PROXY') == 'true') {
                print_r(getenv('WS_REVERSE_PROXY_URL'));
            }

            else {
                $port = getenv('MONITORING_BIND_PORT') ?: '8080';
                print_r(getenv('MONITORING_BIND_HOST') . ':' . $port);
            }
            ?>";

            window.TOKEN = "<?php print_r(base64_encode(getenv('DASHBOARD_USERNAME') . ':' . getenv('DASHBOARD_PASSWORD'))); ?>";

            window.MONITORING_MAX_LOGS = <?php echo getenv('MONITORING_MAX_LOGS') ?: '500'; ?>;
        </script>
        <script src="index.js"></script>
    </body>
</html><?php } ?>
