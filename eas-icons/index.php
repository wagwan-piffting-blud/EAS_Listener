<?php

function get_curl_same_codes($url) {
    $ch = curl_init();
    curl_setopt($ch, CURLOPT_URL, $url);
    curl_setopt($ch, CURLOPT_RETURNTRANSFER, 1);
    curl_setopt($ch, CURLOPT_FOLLOWLOCATION, 1);
    $data = curl_exec($ch);
    curl_close($ch);
    $data_json = json_decode($data, true);
    return $data_json["EVENTS"];
}

function determine_if_cached($url) {
    $cache_file = __DIR__ . '/same_codes_cache.json';
    $cache_time = 30 * 24 * 60 * 60;

    if (file_exists($cache_file) && (time() - filemtime($cache_file) < $cache_time)) {
        $data = file_get_contents($cache_file);
        $data_json = json_decode($data, true);
        return $data_json["EVENTS"];
    }

    else {
        $data_json = get_curl_same_codes($url);
        file_put_contents($cache_file, json_encode(["EVENTS" => $data_json]));
        return $data_json;
    }
}

if(isset($_GET['code']) && isset($_GET['hex'])) {
    $same_codes = determine_if_cached("https://raw.githubusercontent.com/Newton-Communications/E2T/refs/heads/main/EAS2Text/same-us.json");

    $code = strtoupper($_GET['code']);

    if(array_key_exists($code, $same_codes)) {
        $img = $code;
    }

    else {
        die("Invalid SAME code.");
    }

    if(substr($_GET['hex'], 0, 2) == '0x' && preg_match('/^0x[0-9A-Fa-f]{6}$/', $_GET['hex'])) {
        $hex = '#' . substr($_GET['hex'], 2);
    }

    else {
        die("Invalid HEX color. Maybe you forgot the 0x or your color is outside of the HEX range?");
    }

    $filetoload = __DIR__ . "/icons/" . strtolower($img) . ".svg";

    if (file_exists($filetoload)) {
        $svg = file_get_contents($filetoload);
        $svg = str_replace("currentColor", $hex, $svg);

        $svg_im = new Imagick();
        $svg_im->setBackgroundColor(new ImagickPixel('transparent'));
        $svg_im->readImageBlob($svg);

        $svg_width = $svg_im->getImageWidth();
        $svg_height = $svg_im->getImageHeight();

        $canvas_size = max($svg_width, $svg_height) + 20;

        $im = new Imagick();
        $im->newImage($canvas_size, $canvas_size, new ImagickPixel('transparent'));
        $im->setImageFormat("png");

        $x_offset = ($canvas_size - $svg_width) / 2;
        $y_offset = ($canvas_size - $svg_height) / 2;

        $im->compositeImage($svg_im, Imagick::COMPOSITE_OVER, $x_offset, $y_offset);

        header("Content-Type: image/png");

        echo $im;

        $svg_im->destroy();
        $im->destroy();
    }

    else {
        die("Event icon not found.");
    }

}

else {
    echo "EAS Icon API v1.0 by Wags";
}
