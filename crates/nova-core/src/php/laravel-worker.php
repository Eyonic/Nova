<?php
// NOVA's Laravel worker (`[site.php] worker = "laravel"`), embedded in the
// nova binary and written next to the site's sockets at start.
//
// It is Laravel Octane's FrankenPHP worker loop (laravel/octane
// bin/frankenphp-worker.php, MIT) running on NOVA's PHP extension, which
// provides frankenphp_handle_request(): PHP fills $_SERVER, $_GET, $_POST,
// $_COOKIE and $_FILES itself, header() and echo reach the client, exactly
// as under PHP-FPM. One addition: generator responses
// (response()->stream(fn () => yield ...)) are streamed chunk by chunk;
// Octane's FrankenPHP client sends them empty under Octane.
//
// Needs only `composer require laravel/octane`.

use Laravel\Octane\ApplicationFactory;
use Laravel\Octane\FrankenPhp\FrankenPhpClient;
use Laravel\Octane\OctaneResponse;
use Laravel\Octane\RequestContext;
use Laravel\Octane\Stream;
use Laravel\Octane\Worker;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\HttpFoundation\StreamedResponse;

if (! function_exists('frankenphp_handle_request')) {
    fwrite(STDERR, "worker = \"laravel\" needs NOVA's PHP extension (extension=nova)\n");
    exit(1);
}

$base = $_SERVER['APP_BASE_PATH'] ?? getcwd();
if (! is_file($base.'/vendor/laravel/octane/bin/bootstrap.php')) {
    fwrite(STDERR, "worker = \"laravel\" needs Laravel Octane: composer require laravel/octane\n");
    exit(11);
}

ignore_user_abort(true);

$basePath = require $base.'/vendor/laravel/octane/bin/bootstrap.php';

final class NovaFrankenPhpClient extends FrankenPhpClient
{
    public function respond(RequestContext $context, OctaneResponse $octaneResponse): void
    {
        $response = $octaneResponse->response;
        $callback = $response instanceof StreamedResponse ? $response->getCallback() : null;
        if ($callback !== null && (new ReflectionFunction($callback))->isGenerator()) {
            $response->sendHeaders();
            foreach ($callback() as $chunk) {
                echo $chunk;
                flush();
            }
            return;
        }
        parent::respond($context, $octaneResponse);
    }
}

$client = new NovaFrankenPhpClient();

try {
    $worker = tap(new Worker(new ApplicationFactory($basePath), $client))->boot();
} catch (Throwable $e) {
    fwrite(STDERR, sprintf("[octane bootstrap] %s: %s\n  in %s:%d\n%s\n",
        $e::class, $e->getMessage(), $e->getFile(), $e->getLine(), $e->getTraceAsString()));
    exit(1);
}

$requestCount = 0;
$maxRequests = (int) ($_ENV['MAX_REQUESTS'] ?? $_SERVER['MAX_REQUESTS'] ?? 1000);
$debugMode = $_ENV['APP_DEBUG'] ?? $_SERVER['APP_DEBUG'] ?? 'false';

$handleRequest = static function () use ($worker, $client, $debugMode) {
    try {
        [$request, $context] = $client->marshalRequest(new RequestContext());
        $worker->handle($request, $context);
    } catch (Throwable $e) {
        report($e);
        (new Response(
            $debugMode === 'true' ? (string) $e : 'Internal Server Error',
            500,
            ['Content-Type' => 'text/plain'],
        ))->send();
        Stream::shutdown($e);
    }
};

try {
    while ($requestCount < $maxRequests && frankenphp_handle_request($handleRequest)) {
        $requestCount++;
    }
} finally {
    $worker?->terminate();
    gc_collect_cycles();
}
