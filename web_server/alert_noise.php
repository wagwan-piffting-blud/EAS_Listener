<?php

require_once __DIR__ . "/config.php";

$alertSoundSrc = app_string('ALERT_SOUND_SRC', 'iembot.mp3');
$alertSoundEnabled = app_bool('ALERT_SOUND_ENABLED', false) ? 'true' : 'false';

$finfo = finfo_open(FILEINFO_MIME_TYPE);
$mimeType = finfo_file($finfo, $alertSoundSrc);

echo 'data:' . $mimeType . ';base64,' . base64_encode(file_get_contents($alertSoundSrc));
