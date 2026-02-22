<?php

function get_watched_fips_lookup(): array {
    static $lookup = null;

    if($lookup !== null) {
        return $lookup;
    }

    $lookup = [];
    $watched_fips_env = trim(getenv("WATCHED_FIPS") ?: "");

    if($watched_fips_env === "" || $watched_fips_env === "000000") {
        return $lookup;
    }

    foreach(explode(",", $watched_fips_env) as $watched_fip) {
        $watched_fip = trim($watched_fip);

        if($watched_fip !== "") {
            $lookup[$watched_fip] = true;
        }
    }

    return $lookup;
}

function match_watched_fips(string $locations_string, ?array $watched_fips_lookup = null): bool {
    if($watched_fips_lookup === null) {
        $watched_fips_lookup = get_watched_fips_lookup();
    }

    if(empty($watched_fips_lookup) || $locations_string === "") {
        return false;
    }

    foreach(explode("-", $locations_string) as $alert_fip) {
        $alert_fip = trim($alert_fip);

        if($alert_fip === "") {
            continue;
        }

        if(isset($watched_fips_lookup[$alert_fip])) {
            return true;
        }
    }

    return false;
}

function resolve_id($id) {
    $files = glob(getenv("RECORDING_DIR") . "/EAS_Recording_*.wav");

    usort($files, function($a, $b) {
        return filemtime($a) - filemtime($b);
    });

    if(!isset($files[$id])) {
        return null;
    }

    return $files[$id];
}

function hhmmToSeconds(string $hhmmString): int {
    if (strlen($hhmmString) !== 4 || !ctype_digit($hhmmString)) {
        throw new InvalidArgumentException("Input must be a 4-digit numeric string representing HHMM.");
    }

    $hours = (int) substr($hhmmString, 0, 2);
    $minutes = (int) substr($hhmmString, 2, 2);

    if ($hours < 0 || $minutes < 0 || $minutes >= 60) {
        throw new InvalidArgumentException("Invalid HHMM format. Hours or minutes are out of range.");
    }

    $totalSeconds = ($hours * 3600) + ($minutes * 60);

    return $totalSeconds;
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

$requestHeaders = getallheaders();

if(isset($requestHeaders['Authorization']) && $requestHeaders['Authorization'] === "Bearer " . base64_encode(getenv('DASHBOARD_USERNAME') . ':' . getenv('DASHBOARD_PASSWORD'))) {
    $_SESSION['authed'] = true;
}

if(!isset($_SESSION['authed'])) {
    header("Location: index.php?redirect=" . urlencode($_SERVER['REQUEST_URI']));
    exit();
}

if(isset($_GET["latest_id"]) && $_GET["latest_id"] !== null && $_SESSION['authed'] === true) {
    $files = glob(getenv("RECORDING_DIR") . "/EAS_Recording_*.wav");

    usort($files, function($a, $b) {
        return filemtime($a) - filemtime($b);
    });

    echo count($files) - 1;
    exit();
}

if(isset($_GET["recording_id"]) && $_GET["recording_id"] !== null && $_SESSION['authed'] === true) {
    $file = resolve_id($_GET["recording_id"]);

    if($file === null || $file === false || !file_exists($file)) {
        http_response_code(404);
        echo "File not found.";
        exit();
    }

    $filesize = filesize($file);
    $start = 0;
    $end = $filesize - 1;
    $length = $filesize;
    $range_header = $_SERVER['HTTP_RANGE'] ?? null;

    if($range_header && preg_match('/bytes=(\d*)-(\d*)/', $range_header, $matches)) {
        $range_start = $matches[1] !== '' ? (int) $matches[1] : null;
        $range_end = $matches[2] !== '' ? (int) $matches[2] : null;

        if($range_start === null && $range_end !== null) {
            // suffix range: bytes=-N
            $range_start = $filesize - $range_end;
            $range_end = $end;
        }

        if($range_start !== null && $range_end === null) {
            $range_end = $end;
        }

        if(
            $range_start !== null && $range_end !== null &&
            $range_start <= $range_end && $range_end < $filesize && $range_start >= 0
        ) {
            $start = $range_start;
            $end = $range_end;
            $length = $end - $start + 1;
            http_response_code(206);
            header("Content-Range: bytes $start-$end/$filesize");
        } else {
            header("Content-Range: bytes */$filesize");
            http_response_code(416);
            exit();
        }
    } else {
        http_response_code(200);
    }

    header("Content-Type: audio/wav");
    header('Content-Disposition: inline; filename="' . basename($file) . '"');
    header('Content-Transfer-Encoding: binary');
    header("Accept-Ranges: bytes");
    header("Content-Length: " . $length);

    $handle = fopen($file, 'rb');

    if($handle === false) {
        http_response_code(500);
        echo "Failed to open recording.";
        exit();
    }

    if($start > 0) {
        fseek($handle, $start);
    }

    $bufferSize = 8192;
    $bytesSent = 0;

    while(!feof($handle) && $bytesSent < $length) {
        $bytesToRead = min($bufferSize, $length - $bytesSent);
        $data = fread($handle, $bytesToRead);

        if($data === false) {
            break;
        }

        echo $data;
        $bytesSent += strlen($data);

        if(connection_status() != CONNECTION_NORMAL) {
            break;
        }
    }

    fclose($handle);
    exit();
}

if(!empty($_GET['fetch_alerts']) && $_SESSION['authed'] === true) {
    date_default_timezone_set(getenv("TZ") ?: "UTC");
    header("Content-Type: application/json");

    $alerts_file_path = getenv("SHARED_STATE_DIR") . "/" . getenv("DEDICATED_ALERT_LOG_FILE");
    $max_alerts = 50;
    $alert_lines = [];
    $alertdata = [];
    $included_alert_count = 0;
    $filter_by_watched_fips = ($_GET['filter_alerts'] ?? null) === 'watched_fips';
    $watched_fips_lookup = [];

    if($filter_by_watched_fips) {
        $watched_fips_lookup = get_watched_fips_lookup();

        if(empty($watched_fips_lookup)) {
            $filter_by_watched_fips = false;
        }
    }

    if(is_readable($alerts_file_path)) {
        $handle = fopen($alerts_file_path, "r");

        if($handle !== false) {
            while(($line = fgets($handle)) !== false) {
                $alert_line = trim($line);

                if($alert_line === '') {
                    continue;
                }

                if($filter_by_watched_fips) {
                    if(
                        !preg_match('/^ZCZC-[A-Z0-9]{3}-[A-Z0-9]{3}-([0-9]{6}(?:-[0-9]{6})*)\+/', $alert_line, $zczc_matches)
                        || !match_watched_fips($zczc_matches[1] ?? '', $watched_fips_lookup)
                    ) {
                        continue;
                    }
                }

                $alert_lines[] = [
                    "raw" => $alert_line,
                    "recording_id" => $included_alert_count,
                ];
                $included_alert_count += 1;

                if(count($alert_lines) > $max_alerts) {
                    array_shift($alert_lines);
                }
            }

            fclose($handle);
        }
    }

    foreach($alert_lines as $entry) {
        $alert = $entry["raw"];
        $recording_id = $entry["recording_id"];

        $received_at = preg_match('/\(Received @ (.*?)\)$/', $alert, $matches) ? strtotime($matches[1]) : null;
        $length = preg_match('/\+(\d{4})-/', $alert, $matches) ? $matches[1] : null;
        $length_as_secs = hhmmToSeconds($length);
        $expired_at = $received_at + $length_as_secs;

        $alert_severity_raw = preg_match('/has issued (.*?) for/', $alert, $matches) ? explode(" for ", $matches[1])[0] : null;
        $alert_severity_words_array = preg_split('/(?=[A-Z])/', $alert_severity_raw, -1, PREG_SPLIT_NO_EMPTY);

        if($alert_severity_words_array[3]) {
            $alert_severity = strtolower($alert_severity_words_array[3]);
        }

        else if($alert_severity_words_array[2]) {
            $alert_severity = strtolower($alert_severity_words_array[2]);
        }

        else {
            $alert_severity = strtolower($alert_severity_words_array[1]);
        }

        $alert_processed = [
            "received_at" => $received_at,
            "expired_at" => $expired_at,
            "data" => [
                "event_code" => preg_match('/ZCZC-[A-Z]{3}-([A-Z]{3})-/', $alert, $matches) ? $matches[1] : null,
                "event_text" => preg_match('/has issued a (.*?) for/', $alert, $matches) ? explode(" for ", $matches[1])[0] : null,
                "originator" => preg_match('/Message from (.*?)[.;]/', $alert, $matches) ? $matches[1] : null,
                "locations" => preg_match('/for (.*?); beginning/', $alert, $matches) ? $matches[1] : null,
                "alert_severity" => $alert_severity,
                "length" => $length,
                "raw_zczc" => preg_match('/^(ZCZC-[A-Z]{3}-[A-Z]{3}-((?:\d{6}(?:-?)){1,31})\+\d{4}-\d{7}-[A-Za-z0-9\/ ]{1,8}?-)/', $alert, $matches) ? $matches[1] : null,
                "eas_text" => preg_match('/-: (.*\.) \(/', $alert, $matches) ? $matches[1] : null,
                "audio_recording" => "archive.php?recording_id=" . $recording_id,
            ]
        ];

        $alertdata[] = $alert_processed;
    }

    echo json_encode($alertdata);
    exit();
}

else { ?><!DOCTYPE html>
<html lang="en">
    <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <title>EAS Archived Alerts</title>
        <link rel="stylesheet" href="/style.css" />
    </head>
    <body>
        <header>
            <h1><img src="assets/favicon-96x96.png" alt="EAS Logo" class="logo" />EAS Archived Alerts</h1>
            <div id="header-right">
                <div id="logout-container">
                    <a class="custom-button" href="index.php">Back to Dashboard</a>
                    <a class="custom-button" href="logout.php">Logout</a>
                </div>
            </div>
        </header>
        <main id="oldAlerts">
            <section id="oldAlertSection">
                <h2>
                    <span class="section-title">Archived/Old Alerts</span>
                    <span id="filterStatus" class="pill">Showing All</span>
                    <span id="filterOptions" class="pill">
                        Filter by...
                    </span>
                    <span id="fipsFilterToggle" class="pill" role="button" tabindex="0">
                        Watched FIPS
                    </span>
                    <span id="oldAlertCount" class="pill">None</span>
                </h2>
                <p class="smalltext">Note: Alerts are shown in chronological order (old to new, top to bottom) on this page.</p>
                <div id="oldAlertList" class="section-scroll"></div>
            </section>
        </main>
        <script src="archive.js"></script>
    </body>
</html>
<?php } ?>
