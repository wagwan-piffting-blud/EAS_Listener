<?php

if(!session_id()) {
    if(getenv('USE_REVERSE_PROXY') === 'true') {
        session_set_cookie_params(259200, "/", "", true, true);
    }

    else {
        session_set_cookie_params(259200, "/", "", false, true);
    }
    session_start();
}

$requestHeaders = getallheaders();

if(isset($requestHeaders['Authorization']) && $requestHeaders['Authorization'] === "Bearer " . base64_encode(getenv('DASHBOARD_USERNAME') . ':' . getenv('DASHBOARD_PASSWORD'))) {
    $_SESSION['authed'] = true;
}

if(!isset($_SESSION['authed'])) {
    header("Location: index.php?redirect=" . urlencode($_SERVER['REQUEST_URI']));
    exit();
}

elseif(isset($_SESSION['authed']) && $_SESSION['authed'] === true && isset($_POST['vacuum']) && $_POST['vacuum'] === "true") {
    try {
        $recordingsDir = getenv("RECORDING_DIR");
        $oldDir = $recordingsDir . "/__old__";

        if (!is_dir($oldDir)) {
            mkdir($oldDir, 0755, true);
        }

        $files = glob(getenv("RECORDING_DIR") . "/EAS_Recording_*.wav");
        foreach ($files as $file) {
            $baseName = basename($file);
            rename($file, $oldDir . "/" . $baseName);
        }

        $alerts_file_path = getenv("SHARED_STATE_DIR") . "/" . getenv("DEDICATED_ALERT_LOG_FILE");

        if (file_exists($alerts_file_path)) {
            $backupPath = $alerts_file_path . ".bak";
            if (!file_exists($backupPath)) {
                copy($alerts_file_path, $backupPath);
            }
            else {
                file_put_contents($backupPath, file_get_contents($alerts_file_path), FILE_APPEND);
            }
        }

        file_put_contents($alerts_file_path, "");
        ?><!DOCTYPE html>
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
                <h1>The vacuuming process has been completed.</h1>
                <p>Old recordings have been moved to the __old__ directory within the recordings directory, and the current alert log has been backed up and truncated. This page will now redirect back to the dashboard in a few seconds.</p>
            </section>
        </main>
        <script>
            setTimeout(function() {
                window.location.href = "index.php";
            }, 7000);
        </script>
    </body>
</html><?php } catch (Exception $e) {
        die("Error during vacuuming: " . $e->getMessage());
    }
}

else { ?><!DOCTYPE html>
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
                <h1>Are you sure you want to vacuum old recordings and truncate the alert log?</h1>
                <p>This action will move all existing recordings to an __old__ subdirectory and clear the current alert log. It is recommended to back up any important recordings or alert data within your data directory before proceeding. This will <strong>not delete</strong> any data, but it will move recordings and back up the alert log as described.</p>
                <form method="POST" action="vacuum.php">
                    <input type="hidden" name="vacuum" value="true" />
                    <button type="submit" class="button-danger">Yes, vacuum old recordings and truncate alert log</button>
                    <button type="button" onclick="window.location.href='index.php'" class="button-safety">No, cancel</button>
                </form>
            </section>
        </main>
    </body>
</html><?php } ?>
