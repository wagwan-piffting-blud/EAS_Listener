<?php

require_once __DIR__ . "/config.php";

function get_active_alert_headers_lookup(): array {
    $lookup = [];
    $shared_state_dir = app_shared_state_dir();
    if($shared_state_dir === "") {
        return $lookup;
    }

    $active_alerts_path = $shared_state_dir . DIRECTORY_SEPARATOR . "active_alerts.json";
    if(!is_readable($active_alerts_path)) {
        return $lookup;
    }

    $raw_payload = @file_get_contents($active_alerts_path);
    if($raw_payload === false || $raw_payload === "") {
        return $lookup;
    }

    $decoded = json_decode($raw_payload, true);
    if(!is_array($decoded)) {
        return $lookup;
    }

    $now = time();
    foreach($decoded as $entry) {
        if(!is_array($entry)) {
            continue;
        }

        $raw_header = trim((string) ($entry["raw_header"] ?? ""));
        if($raw_header === "") {
            continue;
        }

        $expires_at = isset($entry["expires_at"]) ? (int) $entry["expires_at"] : null;
        if($expires_at !== null && $expires_at <= $now) {
            continue;
        }

        $lookup[$raw_header] = true;
    }

    return $lookup;
}

function extract_logged_raw_header(string $alert_line): string {
    $parts = explode(": ", $alert_line, 2);
    if(count($parts) !== 2) {
        return "";
    }

    return trim($parts[0]);
}

function get_recording_files_sorted(string $recordings_dir): array {
    $pattern = rtrim($recordings_dir, "/\\") . DIRECTORY_SEPARATOR . "EAS_Recording_*.wav";
    $files = glob($pattern);
    if($files === false || empty($files)) {
        return [];
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

function is_finalized_wav_recording(string $file): bool {
    $filesize = @filesize($file);
    if($filesize === false || $filesize < 12) {
        return false;
    }

    $handle = @fopen($file, "rb");
    if($handle === false) {
        return false;
    }

    $riff_header = @fread($handle, 12);
    if($riff_header === false || strlen($riff_header) < 12) {
        fclose($handle);
        return false;
    }

    if(substr($riff_header, 0, 4) !== "RIFF" || substr($riff_header, 8, 4) !== "WAVE") {
        fclose($handle);
        return false;
    }

    $riff_size = (int) unpack("V", substr($riff_header, 4, 4))[1];
    $data_offset = null;
    $data_size = null;
    $offset = 12;

    while(($offset + 8) <= $filesize) {
        if(@fseek($handle, $offset) !== 0) {
            break;
        }

        $chunk_header = @fread($handle, 8);
        if($chunk_header === false || strlen($chunk_header) < 8) {
            break;
        }

        $chunk_id = substr($chunk_header, 0, 4);
        $chunk_size = (int) unpack("V", substr($chunk_header, 4, 4))[1];
        $chunk_data_offset = $offset + 8;
        $chunk_end = $chunk_data_offset + $chunk_size;

        if($chunk_end > $filesize) {
            break;
        }

        if($chunk_id === "data") {
            $data_offset = $chunk_data_offset;
            $data_size = $chunk_size;
            break;
        }

        $offset = $chunk_end + ($chunk_size % 2);
    }

    fclose($handle);
    if($data_offset === null || $data_size === null) {
        return false;
    }

    $expected_data_size_max = (int) $filesize - $data_offset;
    $expected_riff_size = (int) $filesize - 8;

    return $riff_size === $expected_riff_size
        && $data_size >= 0
        && $data_size <= $expected_data_size_max;
}

function parse_alert_log_entries(string $alerts_file_path): array {
    if(!is_readable($alerts_file_path)) {
        return [];
    }

    $raw_payload = @file_get_contents($alerts_file_path);
    if($raw_payload === false || trim($raw_payload) === "") {
        return [];
    }

    $normalized = str_replace("\r\n", "\n", $raw_payload);
    $chunks = preg_split("/\n{2,}/", trim($normalized));
    if(!is_array($chunks) || empty($chunks)) {
        return [];
    }

    $entries = [];
    foreach($chunks as $chunk) {
        $line = trim($chunk);
        if($line !== "") {
            $entries[] = $line;
        }
    }

    return $entries;
}

function build_alert_log_payload(array $entries): string {
    if(empty($entries)) {
        return "";
    }

    return implode("\n\n", $entries) . "\n\n";
}

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
    exit();
}

elseif(isset($_SESSION['authed']) && $_SESSION['authed'] === true && isset($_POST['vacuum']) && $_POST['vacuum'] === "true") {
    try {
        $recordingsDir = app_recording_dir();
        $oldDir = $recordingsDir . "/__old__";
        $alerts_file_path = app_dedicated_alert_log_path();
        $active_headers_lookup = get_active_alert_headers_lookup();
        $recording_files = get_recording_files_sorted($recordingsDir);
        $alerts_entries = parse_alert_log_entries($alerts_file_path);
        $keep_recordings_lookup = [];
        $retained_alert_entries = [];

        if (!is_dir($oldDir)) {
            mkdir($oldDir, 0755, true);
        }

        foreach($recording_files as $file) {
            if(!is_finalized_wav_recording($file)) {
                $keep_recordings_lookup[basename($file)] = true;
            }
        }

        $active_last_indexes = [];
        foreach($alerts_entries as $index => $entry) {
            $logged_raw_header = extract_logged_raw_header($entry);
            if($logged_raw_header === "" || !isset($active_headers_lookup[$logged_raw_header])) {
                continue;
            }

            $active_last_indexes[$logged_raw_header] = $index;
        }

        $retained_indexes = array_values($active_last_indexes);
        sort($retained_indexes, SORT_NUMERIC);

        foreach($retained_indexes as $index) {
            if(!isset($alerts_entries[$index])) {
                continue;
            }

            $retained_alert_entries[] = $alerts_entries[$index];
            if(isset($recording_files[$index])) {
                $keep_recordings_lookup[basename($recording_files[$index])] = true;
            }
        }

        foreach($recording_files as $file) {
            $baseName = basename($file);
            if(isset($keep_recordings_lookup[$baseName])) {
                continue;
            }

            rename($file, $oldDir . "/" . $baseName);
        }

        if (file_exists($alerts_file_path)) {
            $backupPath = $alerts_file_path . ".bak";
            if (!file_exists($backupPath)) {
                copy($alerts_file_path, $backupPath);
            }
            else {
                file_put_contents($backupPath, file_get_contents($alerts_file_path), FILE_APPEND);
            }
        }

        file_put_contents($alerts_file_path, build_alert_log_payload($retained_alert_entries));
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
                <p>This action will move all existing, not active recordings to an __old__ subdirectory and clear the current alert log of non-active alerts. It is recommended to back up any important recordings or alert data within your data directory before proceeding. This will <strong>not delete</strong> any data, but it will move recordings and back up the alert log as described.</p>
                <form method="POST" action="vacuum.php">
                    <input type="hidden" name="vacuum" value="true" />
                    <button type="button" class="button-danger" id="vacuumButton">Yes, vacuum old recordings and truncate alert log</button>
                    <button type="button" onclick="window.location.href='index.php'" class="button-safety" id="cancelButton">No, cancel</button>
                </form>
            </section>
        </main>
        <script>
            document.getElementById("vacuumButton").addEventListener("click", function() {
                if(confirm("Are you absolutely sure you want to vacuum old recordings and truncate the alert log? This action cannot be undone automatically!")) {
                    document.getElementById("vacuumButton").disabled = true;
                    document.getElementById("cancelButton").disabled = true;
                    document.getElementById("vacuumButton").textContent = "Vacuuming in progress...";
                    this.closest("form").submit();
                }
            });
        </script>
    </body>
</html><?php } ?>
