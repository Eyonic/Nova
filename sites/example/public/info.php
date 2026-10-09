<?php
// Reports what PHP sees for this request; used by the integration tests.
header('Content-Type: application/json');
$files = [];
foreach ($_FILES as $name => $f) {
    $files[$name] = ['size' => $f['size'], 'error' => $f['error'], 'sha1' => $f['error'] === 0 ? sha1_file($f['tmp_name']) : null];
}
echo json_encode([
    'php' => PHP_VERSION,
    'sapi' => PHP_SAPI,
    'site' => getenv('NOVA_SITE'),
    'app_name' => getenv('APP_NAME'),
    'method' => $_SERVER['REQUEST_METHOD'],
    'script_name' => $_SERVER['SCRIPT_NAME'],
    'path_info' => $_SERVER['PATH_INFO'] ?? null,
    'request_uri' => $_SERVER['REQUEST_URI'],
    'query' => $_GET,
    'post' => $_POST,
    'files' => $files,
    'raw_body_bytes' => strlen(file_get_contents('php://input')),
    'request_id' => $_SERVER['NOVA_REQUEST_ID'] ?? null,
    'https' => $_SERVER['HTTPS'] ?? null,
    'remote_addr' => $_SERVER['REMOTE_ADDR'] ?? null,
    'opcache' => function_exists('opcache_get_status') && (opcache_get_status(false)['opcache_enabled'] ?? false),
    'extensions' => array_values(array_intersect(['pdo_mysql', 'mysqli', 'Zend OPcache'], get_loaded_extensions(false) + get_loaded_extensions(true))),
], JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES);
