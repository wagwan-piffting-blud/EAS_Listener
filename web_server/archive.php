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

function get_recording_dir(): string {
    return rtrim((string) (getenv("RECORDING_DIR") ?: ""), "/\\");
}

function get_recording_manifest_path(string $recording_dir): string {
    return $recording_dir . DIRECTORY_SEPARATOR . ".recording_manifest.json";
}

function scan_recording_files(string $recording_dir): array {
    $pattern = $recording_dir . DIRECTORY_SEPARATOR . "EAS_Recording_*.wav";
    $files = glob($pattern);
    if($files === false) {
        $files = [];
    }

    $entries = [];
    foreach($files as $file) {
        $mtime = @filemtime($file);
        if($mtime === false) {
            continue;
        }

        $entries[] = [
            "path" => $file,
            "mtime" => (int) $mtime,
        ];
    }

    usort($entries, function($a, $b) {
        return $a["mtime"] <=> $b["mtime"];
    });

    return array_values(array_map(function($entry) {
        return $entry["path"];
    }, $entries));
}

function build_recording_manifest(string $recording_dir): array {
    $files = scan_recording_files($recording_dir);
    return [
        "version" => 1,
        "generated_at" => time(),
        "directory_mtime" => (int) (@filemtime($recording_dir) ?: 0),
        "count" => count($files),
        "files" => $files,
    ];
}

function read_recording_manifest(string $manifest_path): ?array {
    if(!is_readable($manifest_path)) {
        return null;
    }

    $raw = @file_get_contents($manifest_path);
    if($raw === false || $raw === "") {
        return null;
    }

    $decoded = json_decode($raw, true);
    if(!is_array($decoded) || !isset($decoded["files"]) || !is_array($decoded["files"])) {
        return null;
    }

    $files = [];
    foreach($decoded["files"] as $file) {
        if(is_string($file) && $file !== "") {
            $files[] = $file;
        }
    }

    $decoded["files"] = array_values($files);
    $decoded["count"] = count($decoded["files"]);
    $decoded["directory_mtime"] = (int) ($decoded["directory_mtime"] ?? 0);
    return $decoded;
}

function write_recording_manifest(string $manifest_path, array $manifest): void {
    $payload = json_encode($manifest, JSON_UNESCAPED_SLASHES);
    if($payload === false) {
        return;
    }

    $tmp_path = $manifest_path . ".tmp." . getmypid() . "." . mt_rand(1000, 9999);
    $handle = @fopen($tmp_path, "wb");
    if($handle === false) {
        return;
    }

    $write_ok = false;
    if(@flock($handle, LOCK_EX)) {
        $written = @fwrite($handle, $payload);
        @fflush($handle);
        @flock($handle, LOCK_UN);
        $write_ok = ($written !== false);
    }

    fclose($handle);

    if(!$write_ok) {
        @unlink($tmp_path);
        return;
    }

    if(!@rename($tmp_path, $manifest_path)) {
        @unlink($tmp_path);
    }
}

function get_recording_manifest(bool $force_refresh = false): array {
    static $request_cache = null;

    if(!$force_refresh && is_array($request_cache)) {
        return $request_cache;
    }

    $recording_dir = get_recording_dir();
    if($recording_dir === "" || !is_dir($recording_dir)) {
        $request_cache = [
            "version" => 1,
            "generated_at" => time(),
            "directory_mtime" => 0,
            "count" => 0,
            "files" => [],
        ];
        return $request_cache;
    }

    $manifest_path = get_recording_manifest_path($recording_dir);
    $directory_mtime = (int) (@filemtime($recording_dir) ?: 0);
    $manifest = null;

    if(!$force_refresh) {
        $manifest = read_recording_manifest($manifest_path);
        if(
            $manifest !== null
            && (int) ($manifest["directory_mtime"] ?? -1) !== $directory_mtime
        ) {
            $manifest = null;
        }
    }

    if($manifest === null) {
        $manifest = build_recording_manifest($recording_dir);
        write_recording_manifest($manifest_path, $manifest);
    }

    $request_cache = $manifest;
    return $request_cache;
}

function resolve_id($id) {
    $recording_id = filter_var($id, FILTER_VALIDATE_INT, [
        "options" => [
            "min_range" => 0,
        ],
    ]);
    if($recording_id === false) {
        return null;
    }

    $manifest = get_recording_manifest();
    if(!isset($manifest["files"][$recording_id])) {
        return null;
    }

    return $manifest["files"][$recording_id];
}

function get_latest_recording_id(): int {
    $manifest = get_recording_manifest();
    return (int) ($manifest["count"] ?? 0) - 1;
}

function is_finalized_wav_recording(string $file): bool {
    $filesize = @filesize($file);
    if($filesize === false || $filesize < 44) {
        return false;
    }

    $handle = @fopen($file, "rb");
    if($handle === false) {
        return false;
    }

    $header = @fread($handle, 44);
    fclose($handle);

    if($header === false || strlen($header) < 44) {
        return false;
    }

    if(substr($header, 0, 4) !== "RIFF" || substr($header, 8, 4) !== "WAVE") {
        return false;
    }

    // hound writes canonical PCM WAV where "data" chunk begins at offset 36.
    if(substr($header, 36, 4) !== "data") {
        return false;
    }

    $riff_size = unpack("V", substr($header, 4, 4))[1];
    $data_size = unpack("V", substr($header, 40, 4))[1];
    $expected_riff_size = (int) $filesize - 8;
    $expected_data_size = (int) $filesize - 44;

    return $riff_size === $expected_riff_size && $data_size === $expected_data_size;
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

if(session_status() === PHP_SESSION_ACTIVE) {
    session_write_close();
}

if(isset($_GET["latest_id"]) && $_GET["latest_id"] !== null && $_SESSION['authed'] === true) {
    echo get_latest_recording_id();
    exit();
}

if(isset($_GET["recording_id"]) && $_GET["recording_id"] !== null && $_SESSION['authed'] === true) {
    $is_head_request = ($_SERVER['REQUEST_METHOD'] ?? 'GET') === 'HEAD';
    $file = resolve_id($_GET["recording_id"]);

    if($file === null || $file === false || !file_exists($file)) {
        http_response_code(404);
        if(!$is_head_request) {
            echo "File not found.";
        }
        exit();
    }

    if(!is_finalized_wav_recording($file)) {
        http_response_code(425);
        if(!$is_head_request) {
            echo "Recording is still in progress.";
        }
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

    if($is_head_request) {
        exit();
    }

    $handle = fopen($file, 'rb');

    if($handle === false) {
        http_response_code(500);
        if(!$is_head_request) {
            echo "Failed to open recording.";
        }
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
