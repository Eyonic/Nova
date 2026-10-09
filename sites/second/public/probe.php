<?php
// Adversarial probe: everything here must fail for site isolation to hold.
header('Content-Type: application/json');
set_error_handler(fn() => true);
$attempt = function (callable $f) { try { $r = $f(); return $r !== false && $r !== null; } catch (Throwable $e) { return false; } };
$other = '/srv/sites/example';
$results = [
    'read_other_site_file' => $attempt(fn() => file_get_contents("$other/config/secret.php")),
    'include_other_site_file' => $attempt(fn() => include "$other/config/secret.php"),
    'list_other_site_dir' => $attempt(fn() => scandir($other)),
    'read_proc_environ' => $attempt(fn() => file_get_contents('/proc/1/environ')),
    'read_fpm_master_environ' => $attempt(fn() => file_get_contents('/proc/' . posix_getppid() . '/environ')),
    'read_etc_passwd' => $attempt(fn() => file_get_contents('/etc/passwd')),
    'read_nova_state_of_other_site' => $attempt(fn() => scandir('/var/lib/nova/sites/example')),
    'shell_exec' => $attempt(fn() => function_exists('shell_exec') ? shell_exec('id') : false),
    'proc_open' => $attempt(fn() => function_exists('proc_open') ? proc_open('id', [], $p) : false),
    'other_db_with_own_credentials' => $attempt(function () {
        $pdo = new PDO('mysql:host=' . getenv('DB_HOST') . ';dbname=nova_example', getenv('DB_USERNAME'), getenv('DB_PASSWORD'));
        return $pdo->query('SELECT 1')->fetchColumn();
    }),
    // FastCGI hop: talk to another site's PHP-FPM socket to run code as that site.
    'connect_other_site_fpm_socket' => $attempt(fn() => stream_socket_client('unix:///run/nova/php/example/fpm.sock', $en, $es, 1)),
    'list_php_socket_dirs' => $attempt(fn() => scandir('/run/nova/php')),
    // SSRF to NOVA itself, the internet, and opening a listening port.
    'connect_nova_http_port' => $attempt(fn() => fsockopen('127.0.0.1', 8080, $en, $es, 1)),
    'connect_internet_https' => $attempt(fn() => fsockopen('1.1.1.1', 443, $en, $es, 2)),
    'listen_tcp_port' => $attempt(fn() => stream_socket_server('tcp://0.0.0.0:9999', $en, $es)),
    'write_tmp' => $attempt(fn() => file_put_contents('/tmp/nova-probe', 'x')),
    'write_own_project' => $attempt(fn() => file_put_contents(__DIR__ . '/pwned.txt', 'x')),
    'read_worker_cmdline' => $attempt(fn() => file_get_contents('/proc/1/cmdline')),
];
// Signalling other processes: posix_kill is disabled by NOVA's PHP policy;
// the kernel (separate uids + Landlock signal scoping) blocks it underneath.
$results['posix_kill_available'] = function_exists('posix_kill');
$results['uid'] = function_exists('posix_getuid') ? posix_getuid() : null;
// Writing inside its own state directory must keep working (sessions, uploads).
$results['own_state_writable'] = $attempt(fn() => file_put_contents(sys_get_temp_dir() . '/probe-ok', 'x'));
// Everything this site's PHP can see in its environment, for leak checks.
$results['env_dump'] = getenv();
$results['own_db_works'] = $attempt(function () {
    $pdo = new PDO('mysql:host=' . getenv('DB_HOST') . ';dbname=' . getenv('DB_DATABASE'), getenv('DB_USERNAME'), getenv('DB_PASSWORD'));
    return $pdo->query('SELECT 1')->fetchColumn();
});
echo json_encode($results, JSON_PRETTY_PRINT);
