<?php
/**
 * One-time migration script: import dedicated-alerts.log entries into the SQLite database.
 *
 * Usage:
 *   php import_legacy_log.php
 *
 * This script reads the existing dedicated-alerts.log file, parses each entry
 * with the same regex patterns used by archive.php, and inserts them into the
 * alerts SQLite database. It is safe to run multiple times -- duplicate entries
 * (same raw_zczc + received_at) will be skipped.
 */

require_once __DIR__ . "/config.php";

$alerts_file_path = app_dedicated_alert_log_path();
$db_path = app_alert_database_path();

if ($db_path === "") {
    fwrite(STDERR, "Error: ALERT_DATABASE_FILE is not configured.\n");
    exit(1);
}

if ($alerts_file_path === "" || !is_readable($alerts_file_path)) {
    fwrite(STDERR, "Error: Legacy alert log file not found or not readable: {$alerts_file_path}\n");
    exit(1);
}

$raw_payload = @file_get_contents($alerts_file_path);
if ($raw_payload === false || trim($raw_payload) === "") {
    echo "Legacy alert log is empty. Nothing to import.\n";
    exit(0);
}

$normalized = str_replace("\r\n", "\n", $raw_payload);
$chunks = preg_split("/\n{2,}/", trim($normalized));
if (!is_array($chunks) || empty($chunks)) {
    echo "No entries found in legacy alert log.\n";
    exit(0);
}

$entries = [];
foreach ($chunks as $chunk) {
    $line = trim($chunk);
    if ($line !== "") {
        $entries[] = $line;
    }
}

echo "Found " . count($entries) . " entries in legacy alert log.\n";
echo "Opening database: {$db_path}\n";

$db = new SQLite3($db_path);
$db->busyTimeout(10000);

$stmt = $db->prepare(
    "INSERT INTO alerts (raw_zczc, eas_text, event_code, event_text, originator_name, locations, duration_hhmm, received_at, source_type)
     VALUES (:raw_zczc, :eas_text, :event_code, :event_text, :originator_name, :locations, :duration_hhmm, :received_at, 'same')"
);

$imported = 0;
$skipped = 0;

$db->exec("BEGIN TRANSACTION");

foreach ($entries as $alert_line) {
    $parts = explode(": ", $alert_line, 2);
    if (count($parts) !== 2) {
        $skipped++;
        continue;
    }

    $raw_zczc = trim($parts[0]);
    $rest = $parts[1];

    $received_at_str = null;
    if (preg_match('/\(Received @ (.*?)\)$/', $alert_line, $m)) {
        $ts = strtotime($m[1]);
        if ($ts !== false) {
            $received_at_str = gmdate("Y-m-d\\TH:i:s\\Z", $ts);
        }
    }

    if ($received_at_str === null) {
        $received_at_str = gmdate("Y-m-d\\TH:i:s\\Z");
    }

    $check = $db->prepare("SELECT COUNT(*) FROM alerts WHERE raw_zczc = :rz AND received_at = :ra");
    $check->bindValue(":rz", $raw_zczc, SQLITE3_TEXT);
    $check->bindValue(":ra", $received_at_str, SQLITE3_TEXT);
    $exists = $check->execute()->fetchArray(SQLITE3_NUM)[0];
    if ($exists > 0) {
        $skipped++;
        continue;
    }

    $eas_text = "";
    if (preg_match('/-: (.*?) \(Received/', $alert_line, $m)) {
        $eas_text = trim($m[1]);
    }

    $event_code = "";
    if (preg_match('/ZCZC-[A-Z]{3}-([A-Z]{3})-/', $raw_zczc, $m)) {
        $event_code = $m[1];
    }

    $event_text = "";
    if (preg_match('/has issued(?: an?| the)? (.*?) for/i', $eas_text, $m)) {
        $event_text = trim($m[1]);
    }

    $originator_name = "";
    if (preg_match('/Message from (.*?)[.;]/', $eas_text, $m)) {
        $originator_name = trim($m[1]);
    }

    $locations = "";
    if (preg_match('/for (.*?); beginning/', $eas_text, $m)) {
        $locations = trim($m[1]);
    }

    $duration_hhmm = null;
    if (preg_match('/\+(\d{4})-/', $raw_zczc, $m)) {
        $duration_hhmm = $m[1];
    }

    $stmt->bindValue(":raw_zczc", $raw_zczc, SQLITE3_TEXT);
    $stmt->bindValue(":eas_text", $eas_text, SQLITE3_TEXT);
    $stmt->bindValue(":event_code", $event_code, SQLITE3_TEXT);
    $stmt->bindValue(":event_text", $event_text, SQLITE3_TEXT);
    $stmt->bindValue(":originator_name", $originator_name, SQLITE3_TEXT);
    $stmt->bindValue(":locations", $locations, SQLITE3_TEXT);
    $stmt->bindValue(":duration_hhmm", $duration_hhmm, $duration_hhmm !== null ? SQLITE3_TEXT : SQLITE3_NULL);
    $stmt->bindValue(":received_at", $received_at_str, SQLITE3_TEXT);
    $stmt->execute();
    $stmt->reset();
    $imported++;
}

$db->exec("COMMIT");
$db->close();

echo "Import complete. Imported: {$imported}, Skipped (duplicates or unparseable): {$skipped}\n";
