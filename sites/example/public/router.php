<?php
// Front controller: receives every request that matches no file.
$path = parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH);
if (preg_match('#^/hello/([\w-]+)$#', $path, $m)) {
    header('Content-Type: application/json');
    echo json_encode(['route' => 'hello', 'name' => $m[1], 'script_name' => $_SERVER['SCRIPT_NAME']]);
    return;
}
http_response_code(404);
header('Content-Type: text/plain');
echo "No route for {$path}\n";
