<?php

/**
 * Runtime notices shown at the top of the dashboard. Driven entirely by
 * image_info.json, which docker_entrypoint.sh rewrites on every container boot,
 * so nothing here needs to be configured by the user.
 */

require_once __DIR__ . "/config.php";

$appDeprecationNotice = app_deprecation_notice();
$appTtsFallbackReason = app_tts_engine_fallback_reason();
$appMigrationUrl = "https://github.com/wagwan-piffting-blud/EAS_Listener#the--lite-image-is-deprecated";

?>
<?php if ($appDeprecationNotice !== "") { ?>
<div class="notice notice-deprecation" role="alert">
    <span class="notice-badge">Deprecated image</span>
    <span class="notice-body">
        <?php echo htmlspecialchars($appDeprecationNotice, ENT_QUOTES); ?>
        <a href="<?php echo htmlspecialchars($appMigrationUrl, ENT_QUOTES); ?>" target="_blank" rel="noopener">Read the migration guide.</a>
    </span>
</div>
<?php } ?>
<?php if ($appTtsFallbackReason !== "") { ?>
<div class="notice notice-warning" role="status">
    <span class="notice-badge">TTS fallback</span>
    <span class="notice-body"><?php echo htmlspecialchars($appTtsFallbackReason, ENT_QUOTES); ?> &mdash; CAP alerts are being voiced with Piper instead.</span>
</div>
<?php } ?>
