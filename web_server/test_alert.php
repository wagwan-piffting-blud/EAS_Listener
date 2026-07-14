<?php

require_once __DIR__ . "/config.php";

if(!session_id()) {
    if(app_use_reverse_proxy()) {
        session_set_cookie_params(259200, "/", "", true, true);
    }

    else {
        session_set_cookie_params(259200, "/", "", false, true);
    }
    session_start();
}

$requestHeaders = getallheaders();

if(app_request_is_authorized($requestHeaders)) {
    $_SESSION['authed'] = true;
}

if(!isset($_SESSION['authed'])) {
    header("Location: index.php?redirect=" . urlencode($_SERVER['REQUEST_URI']));
    exit;
}

if ($_SERVER['REQUEST_METHOD'] === 'GET') {
    $testAlertSignalPath = '/app/test_alert_signal';

    if (file_put_contents($testAlertSignalPath, time()) === false) {
        http_response_code(500);
        echo "Failed to send test alert signal.";
        exit;
    }

    elseif (file_exists($testAlertSignalPath)) { ?><!DOCTYPE html>
<html lang="en">
    <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <title>EAS Monitoring Dashboard</title>
        <link rel="stylesheet" href="/style.css" />
    </head>
    <body>
        <main id="oldAlerts">
            <section id="oldAlertSection">
                <h1>A test alert has been sent to the Rust backend.</h1>
                <p>The backend will inject a synthetic Required Weekly Test (RWT) through the full alert pipeline: decoding, logging, recording, notifications, and any configured relays. Watch the dashboard, your Apprise/Discord notifications, and the archive to confirm each stage. This page will redirect back to the dashboard in a few seconds...</p>
            </section>
        </main>
        <script>
            setTimeout(function() {
                window.location.href = "index.php";
            }, 7000);
        </script>
    </body>
</html><?php }
    else {
        http_response_code(405);
        echo "Method Not Allowed";
    }
}
