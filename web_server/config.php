<?php

$APP_CONFIG = [];
$appConfigSources = [
    "/app/web_config.json",
    __DIR__ . "/web_config.json",
    "/app/config.json",
];

foreach ($appConfigSources as $configPath) {
    if (!is_readable($configPath)) {
        continue;
    }

    $rawPayload = @file_get_contents($configPath);
    if ($rawPayload === false || trim($rawPayload) === "") {
        continue;
    }

    $decoded = json_decode($rawPayload, true);
    if (is_array($decoded)) {
        $APP_CONFIG = $decoded;
        break;
    }
}

if (!function_exists("app_config")) {
    function app_config(string $key, $default = null) {
        global $APP_CONFIG;
        return array_key_exists($key, $APP_CONFIG) ? $APP_CONFIG[$key] : $default;
    }
}

if (!function_exists("app_string")) {
    function app_string(string $key, string $default = ""): string {
        $value = app_config($key, $default);
        if (is_string($value)) {
            return $value;
        }
        if (is_int($value) || is_float($value) || is_bool($value)) {
            return (string) $value;
        }
        return $default;
    }
}

if (!function_exists("app_bool")) {
    function app_bool(string $key, bool $default = false): bool {
        $value = app_config($key, $default);
        if (is_bool($value)) {
            return $value;
        }
        if (is_int($value) || is_float($value)) {
            return ((int) $value) !== 0;
        }
        if (is_string($value)) {
            $normalized = strtolower(trim($value));
            if (in_array($normalized, ["1", "true", "yes", "on"], true)) {
                return true;
            }
            if (in_array($normalized, ["0", "false", "no", "off"], true)) {
                return false;
            }
        }
        return $default;
    }
}

if (!function_exists("app_array")) {
    function app_array(string $key, array $default = []): array {
        $value = app_config($key, $default);
        if (is_array($value)) {
            return $value;
        }
        if (is_string($value)) {
            $decoded = json_decode($value, true);
            if (is_array($decoded)) {
                return $decoded;
            }
        }
        return $default;
    }
}

if (!function_exists("app_use_reverse_proxy")) {
    function app_use_reverse_proxy(): bool {
        return app_bool("USE_REVERSE_PROXY", false);
    }
}

if (!function_exists("app_dashboard_username")) {
    function app_dashboard_username(): string {
        return app_string("DASHBOARD_USERNAME", "admin");
    }
}

if (!function_exists("app_dashboard_password")) {
    function app_dashboard_password(): string {
        return app_string("DASHBOARD_PASSWORD", "password");
    }
}

if (!function_exists("app_auth_token")) {
    function app_auth_token(): string {
        return base64_encode(app_dashboard_username() . ":" . app_dashboard_password());
    }
}

if (!function_exists("app_request_is_authorized")) {
    function app_request_is_authorized(array $requestHeaders): bool {
        $provided = null;
        foreach ($requestHeaders as $name => $value) {
            if (strcasecmp((string) $name, "Authorization") === 0) {
                $provided = (string) $value;
                break;
            }
        }

        if ($provided === null) {
            return false;
        }

        $expected = "Bearer " . app_auth_token();
        return hash_equals($expected, $provided);
    }
}

if (!function_exists("app_monitoring_api_base")) {
    function app_monitoring_api_base(string $httpHost): string {
        if (app_use_reverse_proxy()) {
            return app_string("WS_REVERSE_PROXY_URL", "localhost");
        }

        $hostNoPort = substr($httpHost, 0, strpos($httpHost, ":") ?: strlen($httpHost));
        $port = app_string("MONITORING_BIND_PORT", "8080");
        return $hostNoPort . ":" . $port;
    }
}

if (!function_exists("app_shared_state_dir")) {
    function app_shared_state_dir(): string {
        return rtrim(app_string("SHARED_STATE_DIR", ""), "/\\");
    }
}

if (!function_exists("app_recording_dir")) {
    function app_recording_dir(): string {
        return rtrim(app_string("RECORDING_DIR", ""), "/\\");
    }
}

if (!function_exists("app_dedicated_alert_log_path")) {
    function app_dedicated_alert_log_path(): string {
        $configured = trim(app_string("DEDICATED_ALERT_LOG_FILE", "dedicated-alerts.log"));
        if ($configured === "") {
            return "";
        }

        if (
            str_starts_with($configured, "/")
            || preg_match("/^[A-Za-z]:[\\\\\\/]/", $configured)
        ) {
            return $configured;
        }

        $shared = app_shared_state_dir();
        if ($shared === "") {
            return $configured;
        }

        return $shared . DIRECTORY_SEPARATOR . ltrim($configured, "/\\");
    }
}

