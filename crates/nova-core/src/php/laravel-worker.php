<?php
// NOVA's built-in Laravel worker (`[site.php] worker = "laravel"`), embedded
// in the nova binary and written next to the site's sockets at start.
//
// It runs Laravel Octane's Worker (application sandboxing, listeners,
// flushing between requests) but talks to NOVA directly: the request is
// built from NOVA's context the way PHP-FPM would fill the superglobals,
// and the response is serialized straight from Symfony's Response. Octane's
// RoadRunner path converts every request and response through PSR-7
// instead (12-13% slower here) and needs the spiral/* packages; this needs
// only `composer require laravel/octane`.
//
// Wire format: goridge frames on STDIN/STDOUT (see nova-runtime-php
// goridge.rs). Requests carry RoadRunner's JSON context; multipart bodies
// arrive parsed by NOVA's manager (`parsed`, `uploads`), urlencoded bodies
// raw (parsed here with PHP's own parse_str, raw body kept for signatures).

declare(strict_types=1);

use Illuminate\Foundation\Application;
use Illuminate\Http\Request;
use Laravel\Octane\ApplicationFactory;
use Laravel\Octane\Contracts\Client;
use Laravel\Octane\Octane;
use Laravel\Octane\OctaneResponse;
use Laravel\Octane\RequestContext;
use Laravel\Octane\Worker;
use Symfony\Component\HttpFoundation\BinaryFileResponse;
use Symfony\Component\HttpFoundation\File\UploadedFile;
use Symfony\Component\HttpFoundation\StreamedResponse;

ini_set('display_errors', 'stderr');
$_ENV['APP_RUNNING_IN_CONSOLE'] = false;

$basePath = $_SERVER['APP_BASE_PATH'] ?? $_ENV['APP_BASE_PATH'] ?? getcwd();
if (!is_file($basePath . '/vendor/laravel/octane/src/Worker.php')) {
    fwrite(STDERR, "worker = \"laravel\" needs Laravel Octane: composer require laravel/octane\n");
    exit(11);
}
require $basePath . '/vendor/autoload.php';

final class NovaWire
{
    /** @var resource */
    private static $in;
    /** @var resource */
    private static $out;

    public static function open(): void
    {
        self::$in = fopen('php://stdin', 'rb');
        self::$out = fopen('php://stdout', 'wb');
    }

    private static function read(int $n): ?string
    {
        $buf = '';
        while (strlen($buf) < $n) {
            $chunk = fread(self::$in, $n - strlen($buf));
            if ($chunk === false || $chunk === '') {
                return null;
            }
            $buf .= $chunk;
        }
        return $buf;
    }

    /**
     * The next request as [JSON context, body]; null when NOVA stops this
     * worker (control frame) or the pipe closes.
     *
     * @return array{0:string,1:string}|null
     */
    public static function next(): ?array
    {
        while (true) {
            $head = self::read(12);
            if ($head === null) {
                return null;
            }
            $optionBytes = ((ord($head[0]) & 0x0f) - 3) * 4;
            $length = unpack('V', $head, 2)[1];
            $rest = self::read($optionBytes + $length) ?? '';
            if (ord($head[1]) & 0x01) {             // control frame
                if (str_contains($rest, 'stop')) {
                    return null;
                }
                continue;
            }
            $headerLength = $optionBytes ? unpack('V', $rest)[1] : 0;
            return [
                substr($rest, $optionBytes, $headerLength),
                substr($rest, $optionBytes + $headerLength),
            ];
        }
    }

    /** One response frame; `$more` marks a streamed response as unfinished. */
    public static function send(string $header, string $body, bool $more = false): void
    {
        $head = pack('CCV', 0x10 | 4, 0x08, strlen($header) + strlen($body));
        $head .= pack('V', crc32($head)) . ($more ? "\x01" : "\x00") . "\x00" . pack('V', strlen($header));
        fwrite(self::$out, $head . $header);
        if ($body !== '') {
            fwrite(self::$out, $body);
        }
    }
}

final class NovaClient implements Client
{
    public function marshalRequest(RequestContext $context): array
    {
        return [$context->data['request'], $context];
    }

    public function respond(RequestContext $context, OctaneResponse $octaneResponse): void
    {
        $response = $octaneResponse->response;
        $headers = $response->headers->allPreserveCaseWithoutCookies();
        foreach ($response->headers->getCookies() as $cookie) {
            $headers['Set-Cookie'][] = (string) $cookie;
        }
        $head = json_encode(
            ['status' => $response->getStatusCode(), 'headers' => $headers ?: new stdClass()],
            JSON_UNESCAPED_SLASHES | JSON_INVALID_UTF8_SUBSTITUTE,
        );
        $prefix = (string) $octaneResponse->outputBuffer;

        if (!$response instanceof StreamedResponse && !$response instanceof BinaryFileResponse) {
            NovaWire::send($head, $prefix . $response->getContent());
            return;
        }
        // Streamed bodies leave as they are produced (e.g. server-sent events).
        $first = true;
        ob_start(static function (string $chunk) use (&$first, $head): string {
            if ($chunk !== '') {
                NovaWire::send($first ? $head : '', $chunk, true);
                $first = false;
            }
            return '';
        }, 65536);
        echo $prefix;
        // Under Octane, Laravel hands generator callbacks over as they are
        // (response()->stream(fn () => yield ...)): iterate them here.
        $callback = $response instanceof StreamedResponse ? $response->getCallback() : null;
        if ($callback !== null && (new ReflectionFunction($callback))->isGenerator()) {
            foreach ($callback() as $chunk) {
                echo $chunk;
                ob_flush();
            }
        } else {
            $response->sendContent();
        }
        ob_end_flush();
        NovaWire::send($first ? $head : '', '');
    }

    public function error(Throwable $e, Application $app, Request $request, RequestContext $context): void
    {
        NovaWire::send(
            json_encode(['status' => 500, 'headers' => ['Content-Type' => ['text/plain; charset=utf-8']]]),
            Octane::formatExceptionForClient($e, (bool) $app->make('config')->get('app.debug')),
        );
    }
}

/** NOVA's uploads tree -> UploadedFile objects (test mode: not moved by PHP). */
function nova_uploaded_files(array $tree): array
{
    $files = [];
    foreach ($tree as $key => $node) {
        if (isset($node['tmpName']) && is_string($node['tmpName'])) {
            $files[$key] = $node['error'] === UPLOAD_ERR_NO_FILE
                ? null
                : new UploadedFile($node['tmpName'], $node['name'], $node['mime'], $node['error'], true);
        } else {
            $files[$key] = nova_uploaded_files($node);
        }
    }
    return $files;
}

/** `$_SERVER` for one request, as PHP-FPM would fill it. */
function nova_server(array $base, array $ctx, string $basePath): array
{
    $uri = parse_url($ctx['uri']);
    $https = ($uri['scheme'] ?? 'http') === 'https';
    $server = $base;
    $server['REQUEST_METHOD'] = $ctx['method'];
    $server['REQUEST_URI'] = ($uri['path'] ?? '/') . (isset($uri['query']) ? '?' . $uri['query'] : '');
    $server['QUERY_STRING'] = $ctx['rawQuery'];
    $server['SERVER_PROTOCOL'] = $ctx['protocol'];
    $server['REMOTE_ADDR'] = $ctx['remoteAddr'];
    $server['SERVER_NAME'] = $uri['host'] ?? 'localhost';
    $server['SERVER_PORT'] = (string) ($uri['port'] ?? ($https ? 443 : 80));
    $server['SCRIPT_NAME'] = $server['PHP_SELF'] = '/index.php';
    $server['SCRIPT_FILENAME'] = $basePath . '/public/index.php';
    $server['DOCUMENT_ROOT'] = $basePath . '/public';
    $server['REQUEST_TIME_FLOAT'] = microtime(true);
    $server['REQUEST_TIME'] = (int) $server['REQUEST_TIME_FLOAT'];
    if ($https) {
        $server['HTTPS'] = 'on';
    }
    foreach ($ctx['headers'] as $name => $values) {
        $key = strtoupper(str_replace('-', '_', $name));
        $key = $key === 'CONTENT_TYPE' || $key === 'CONTENT_LENGTH' ? $key : 'HTTP_' . $key;
        $server[$key] = implode(', ', $values);
    }
    return $server;
}

NovaWire::open();
// The process environment, as FPM passes it to every request.
$baseServer = array_diff_key($_SERVER, array_flip(['argv', 'argc', 'PHP_SELF', 'SCRIPT_NAME', 'SCRIPT_FILENAME', 'PATH_TRANSLATED']));
$client = new NovaClient();
$worker = null;

try {
    while ($message = NovaWire::next()) {
        [$header, $body] = $message;
        $ctx = json_decode($header, true, 512, JSON_THROW_ON_ERROR);
        if (isset($ctx['server'])) {
            // NOVA sends what FPM would put in $_SERVER.
            $server = $ctx['server'] + $baseServer;
            $server['REQUEST_TIME_FLOAT'] = microtime(true);
            $server['REQUEST_TIME'] = (int) $server['REQUEST_TIME_FLOAT'];
        } else {
            $server = nova_server($baseServer, $ctx, $basePath);
        }
        parse_str($ctx['rawQuery'], $query);
        $post = [];
        if ($ctx['parsed']) {
            $post = json_decode($body, true) ?: [];
            $body = '';
        } elseif (str_starts_with(strtolower($server['CONTENT_TYPE'] ?? ''), 'application/x-www-form-urlencoded')) {
            parse_str($body, $post);
        }
        $request = new Request($query, $post, [], $ctx['cookies'], nova_uploaded_files($ctx['uploads']), $server, $body);

        $worker ??= tap(new Worker(new ApplicationFactory($basePath), $client))->boot();
        $worker->handle($request, new RequestContext(['request' => $request]));
    }
} catch (Throwable $e) {
    fwrite(STDERR, (string) $e . "\n");
    exit(1);
} finally {
    $worker?->terminate();
}
