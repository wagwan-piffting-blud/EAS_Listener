<?php

# Read the environment variables for the alert sound configuration
$alertSoundSrc = getenv('ALERT_SOUND_SRC') ?: 'iembot.mp3';
$alertSoundEnabled = getenv('ALERT_SOUND_ENABLED') === 'true' ? 'true' : 'false';

# Get mime type for alert sound file to set correct Content-Type header
$finfo = finfo_open(FILEINFO_MIME_TYPE);
$mimeType = finfo_file($finfo, $alertSoundSrc);

# Output the audio file content (Data URI)
echo 'data:' . $mimeType . ';base64,' . base64_encode(file_get_contents($alertSoundSrc));
