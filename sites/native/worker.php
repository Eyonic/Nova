<?php
// A worker on NOVA's PHP extension, with the FrankenPHP worker API
// (`worker_api = "frankenphp"`): PHP fills $_SERVER, $_GET, $_POST,
// $_COOKIE and $_FILES itself, and header(), echo and flush() reach the
// client, exactly as under PHP-FPM. The same script runs unchanged on
// FrankenPHP. Laravel needs none of this: `worker = "laravel"`.

declare(strict_types=1);

$served = 0;
$leave = false;

$handler = static function () use (&$served, &$leave): void {
    $served++;
    $path = parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH);
    switch ($path) {
        case '/stream':
            header('Content-Type: text/plain');
            foreach (['one,', 'two,', 'three'] as $part) {
                echo $part;
                flush();
            }
            return;
        case '/sleep':
            usleep((int) (($_GET['s'] ?? 1) * 1e6));
            echo 'slept';
            return;
        case '/fatal':
            // A fatal error ends the worker in the middle of the request.
            $a = [];
            for (;;) {
                $a[] = str_repeat('x', 1 << 20);
            }
        case '/leave':
            // The worker leaves its loop after answering (as Octane's does
            // after MAX_REQUESTS): requests already sent its way go elsewhere.
            $leave = true;
            echo 'leaving';
            return;
        default:
            header('Content-Type: application/json');
            header('X-Served-By: native-worker');
            $uploads = [];
            foreach ($_FILES as $file) {
                array_walk_recursive($file['tmp_name'], function ($tmp) use (&$uploads) {
                    if ($tmp !== '') {
                        $uploads[] = md5_file($tmp);
                    }
                });
            }
            echo json_encode([
                'pid' => getmypid(),
                'served' => $served,
                'method' => $_SERVER['REQUEST_METHOD'],
                'uri' => $_SERVER['REQUEST_URI'],
                'https' => $_SERVER['HTTPS'] ?? null,
                'get' => $_GET,
                'post' => $_POST,
                'cookies' => $_COOKIE,
                'uploads' => $uploads,
                'raw' => file_get_contents('php://input'),
                'agent' => $_SERVER['HTTP_USER_AGENT'] ?? null,
                'tmp_files' => count(glob(sys_get_temp_dir() . '/*') ?: []),
            ]);
    }
};

while (frankenphp_handle_request($handler)) {
    if ($leave) {
        break;
    }
}
