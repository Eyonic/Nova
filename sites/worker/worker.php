<?php
// A minimal worker-mode application without any library: it speaks the
// RoadRunner worker protocol (goridge frames on STDIN/STDOUT) directly.
// Real applications use Laravel Octane, a Symfony runtime or
// spiral/roadrunner-http instead; NOVA runs those unchanged.
//
// State survives between requests ($served below): that is the point of
// worker mode, and why application code must not leak per-request state.

declare(strict_types=1);

$served = 0;

function read_exact($in, int $n): ?string
{
    $buf = '';
    while (strlen($buf) < $n) {
        $chunk = fread($in, $n - strlen($buf));
        if ($chunk === false || $chunk === '') {
            return null;
        }
        $buf .= $chunk;
    }
    return $buf;
}

/** @return array{flags:int, byte10:int, options:list<int>, payload:string}|null */
function read_frame($in): ?array
{
    $head = read_exact($in, 12);
    if ($head === null) {
        return null;
    }
    $words = ord($head[0]) & 0x0f;
    $len = unpack('V', substr($head, 2, 4))[1];
    $rest = read_exact($in, ($words - 3) * 4 + $len) ?? '';
    $options = $words > 3 ? array_values(unpack('V*', substr($rest, 0, ($words - 3) * 4))) : [];
    return [
        'flags' => ord($head[1]),
        'byte10' => ord($head[10]),
        'options' => $options,
        'payload' => substr($rest, ($words - 3) * 4),
    ];
}

function send(string $header, string $body, int $byte10 = 0, int $flags = 0): void
{
    $payload = $header . $body;
    $head = pack('CCV', 0x10 | 4, $flags, strlen($payload));
    $head .= pack('V', crc32($head)) . chr($byte10) . chr(0) . pack('V', strlen($header));
    fwrite(STDOUT, $head . $payload);
}

function respond(int $status, string $body, array $headers = ['Content-Type' => ['application/json']]): void
{
    send(json_encode(['status' => $status, 'headers' => $headers ?: new stdClass()]), $body);
}

while ($frame = read_frame(STDIN)) {
    if ($frame['flags'] & 0x01) {               // control frame
        if (str_contains($frame['payload'], 'stop')) {
            exit(0);                            // NOVA recycles this worker
        }
        continue;
    }
    $n = $frame['options'][0] ?? 0;
    $ctx = json_decode(substr($frame['payload'], 0, $n), true);
    $body = substr($frame['payload'], $n);
    $path = parse_url($ctx['uri'], PHP_URL_PATH);
    parse_str($ctx['rawQuery'], $query);
    $served++;

    switch ($path) {
        case '/die':
            exit(1);
        case '/sleep':
            usleep((int) (($query['s'] ?? 1) * 1e6));
            respond(200, 'slept');
            break;
        case '/error':
            send('', 'something failed', 0, 0x40);
            break;
        case '/stream':
            foreach (['one,', 'two,'] as $part) {
                send(json_encode(['status' => 200, 'headers' => ['Content-Type' => ['text/plain']]]), $part, 0x01);
            }
            respond(200, 'three', ['Content-Type' => ['text/plain']]);
            break;
        default:
            $uploads = [];
            array_walk_recursive($ctx['uploads'], function ($v, $k) use (&$uploads) {
                if ($k === 'tmpName' && $v !== '') {
                    $uploads[] = md5_file($v);
                }
            });
            respond(200, json_encode([
                'pid' => getmypid(),
                'served' => $served,
                'method' => $ctx['method'],
                'uri' => $ctx['uri'],
                'remote' => $ctx['remoteAddr'],
                'query' => $query,
                'post' => $ctx['parsed'] ? json_decode($body, true) : null,
                'raw' => $ctx['parsed'] ? null : $body,
                'uploads' => $uploads,
                'cookies' => $ctx['cookies'],
                'agent' => $ctx['headers']['User-Agent'][0] ?? null,
                'tmp_files' => count(glob(sys_get_temp_dir() . '/*') ?: []),
            ]));
    }
}
