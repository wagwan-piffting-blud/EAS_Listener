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

else { ?><!DOCTYPE html>
<html lang="en">
    <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <title>EAS Monitoring Dashboard</title>
        <link rel="stylesheet" href="/style.css" />
    </head>
    <body>
        <main
            id="chargen"
            data-api-base="<?php
                if(getenv('USE_REVERSE_PROXY') == 'true') {
                    print_r(getenv('WS_REVERSE_PROXY_URL'));
                }

                else {
                    print_r(substr($_SERVER['HTTP_HOST'], 0, strpos($_SERVER['HTTP_HOST'], ':') ?: strlen($_SERVER['HTTP_HOST'])) . ":" . getenv('MONITORING_BIND_PORT') ?: '8080');
                }
            ?>"
            data-token="<?php print_r(base64_encode(getenv('DASHBOARD_USERNAME') . ':' . getenv('DASHBOARD_PASSWORD'))); ?>"
            data-default-text="EAS DETAILS CHANNEL"
        >
            <div id="cgControls" aria-label="Character generator options">
                <div id="flex">
                    <button id="cgControlsToggle" type="button" aria-controls="cgControlsForm" aria-expanded="false">Show Options</button>
                    <button id="backToDashboard" type="button" onclick="window.location.href='index.php'">Back to Dashboard</button>
                </div>
                <form id="cgControlsForm" hidden>
                    <label for="cgBgColor">Background</label>
                    <input id="cgBgColor" name="bgColor" type="color" value="#000000" />

                    <label for="cgTextColor">Text</label>
                    <input id="cgTextColor" name="textColor" type="color" value="#f8f8f8" />

                    <label for="cgAccentColor">Accent</label>
                    <input id="cgAccentColor" name="accentColor" type="color" value="#c8102e" />

                    <label for="cgFontFamily">Font</label>
                    <input id="cgFontFamily" name="fontFamily" type="text" value="Courier New, Courier, monospace" />

                    <label for="cgFontSize">Size</label>
                    <input id="cgFontSize" name="fontSize" type="number" min="14" max="180" step="1" value="56" />

                    <label for="cgFontWeight">Weight</label>
                    <input id="cgFontWeight" name="fontWeight" type="number" min="100" max="900" step="100" value="700" />

                    <label for="cgSpeed">Speed (s)</label>
                    <input id="cgSpeed" name="speed" type="number" min="4" max="120" step="1" value="18" />

                    <label for="cgGap">Gap (rem)</label>
                    <input id="cgGap" name="gap" type="number" min="1" max="300" step="1" value="4" />

                    <label for="cgTextShadow">Shadow</label>
                    <input id="cgTextShadow" name="textShadow" type="text" value="0 0 12px rgba(0, 0, 0, 0.7)" />

                    <label for="cgUppercase">Uppercase</label>
                    <input id="cgUppercase" name="uppercase" type="checkbox" checked />

                    <button id="cgResetStyle" type="button">Reset Style</button>
                </form>
            </div>
            <div id="cgFrame" aria-live="polite">
                <div id="cgViewport">
                    <div id="cgTrack">
                        <span id="cgTextPrimary">EAS DETAILS CHANNEL</span>
                        <span id="cgTextClone" aria-hidden="true">EAS DETAILS CHANNEL</span>
                    </div>
                </div>
                <audio id="cgAudio" preload="none"></audio>
            </div>
        </main>
        <script src="chargen.js"></script>
    </body>
</html><?php } ?>
