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
    $reloadSignalPath = '/app/reload_signal';

    if (file_put_contents($reloadSignalPath, time()) === false) {
        http_response_code(500);
        echo "Failed to send reload signal.";
        exit;
    }

    elseif (file_exists($reloadSignalPath)) { ?><!DOCTYPE html>
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
                <h1>The reload signal has been sent to the Rust backend.</h1>
                <p>The Rust backend will reload its configuration and adjust active streams momentarily. This page will now redirect back to the dashboard in a few seconds...</p>
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
