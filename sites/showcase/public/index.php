<?php
$t0 = hrtime(true);
session_start();
$_SESSION['visits'] = ($_SESSION['visits'] ?? 0) + 1;

$images = [];
foreach (glob(__DIR__ . '/images/*.{jpg,jpeg,png,webp,JPG,JPEG,PNG,WEBP}', GLOB_BRACE) as $file) {
    $size = getimagesize($file);
    if (!$size) {
        continue; // not a readable image
    }
    [$w, $h] = $size;
    // rawurlencode: file names may contain spaces, parentheses, etc.
    $images[] = ['url' => '/images/' . rawurlencode(basename($file)), 'name' => pathinfo($file, PATHINFO_FILENAME),
                 'w' => $w, 'h' => $h, 'bytes' => filesize($file), 'type' => strtoupper(pathinfo($file, PATHINFO_EXTENSION))];
}
$hero = array_values(array_filter($images, fn($i) => $i['name'] === 'aurora'))[0] ?? $images[0];
$kb = fn(int $b) => number_format($b / 1024, 0) . ' KB';
$e = fn($s) => htmlspecialchars((string) $s, ENT_QUOTES);
?>
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Hello, NOVA</title>
  <link rel="stylesheet" href="/assets/style.css">
</head>
<body>
<header class="hero">
  <img class="hero-img" src="<?= $e($hero['url']) ?>?w=1280"
       srcset="<?= $e($hero['url']) ?>?w=640 640w, <?= $e($hero['url']) ?>?w=1280 1280w, <?= $e($hero['url']) ?>?w=1920 1920w, <?= $e($hero['url']) ?> <?= $hero['w'] ?>w"
       sizes="100vw" alt="" fetchpriority="high">
  <div class="hero-text">
    <p class="eyebrow">Static · PHP <?= $e(PHP_VERSION) ?> · AVIF/WebP</p>
    <h1>Hello, world.</h1>
    <p class="lead">This page was rendered by PHP running under NOVA. The images were
      optimized in the background and are served in the best format your browser accepts.</p>
  </div>
</header>

<main>
  <section class="panel stats">
    <div><span class="num"><?= (int) $_SESSION['visits'] ?></span><span class="lbl">your visits (PHP session)</span></div>
    <div><span class="num" id="dl-total">…</span><span class="lbl">image bytes your browser downloaded</span></div>
    <div><span class="num" id="dl-orig"><?= $kb(array_sum(array_column($images, 'bytes'))) ?></span><span class="lbl">original size of all images</span></div>
    <div><span class="num" id="dl-saved">…</span><span class="lbl">saved</span></div>
  </section>

  <div class="bar">
    <h2>Gallery</h2>
    <button id="remeasure" type="button">Re-measure</button>
  </div>
  <p class="hint">Each card asks the server for the same URL with different <code>Accept</code> headers and widths,
    and shows what NOVA actually sends. If a card says <em>original</em> everywhere, the optimizer is still working: wait a few seconds and re-measure.</p>

  <section class="grid">
  <?php foreach ($images as $img): ?>
    <article class="card" data-url="<?= $e($img['url']) ?>" data-bytes="<?= $img['bytes'] ?>">
      <div class="frame<?= $img['type'] === 'PNG' ? ' checker' : '' ?>">
        <img loading="lazy" src="<?= $e($img['url']) ?>?w=640"
             srcset="<?= $e($img['url']) ?>?w=320 320w, <?= $e($img['url']) ?>?w=640 640w, <?= $e($img['url']) ?>?w=960 960w"
             sizes="(max-width: 700px) 100vw, 420px"
             width="<?= $img['w'] ?>" height="<?= $img['h'] ?>" alt="<?= $e($img['name']) ?>">
      </div>
      <div class="meta">
        <h3><?= $e(ucfirst($img['name'])) ?></h3>
        <p><?= $img['w'] ?>×<?= $img['h'] ?> · <?= $e($img['type']) ?> · <?= $kb($img['bytes']) ?> original</p>
        <table class="measure"><tbody><tr><td colspan="3" class="muted">measuring…</td></tr></tbody></table>
        <p class="got muted">Your browser: …</p>
      </div>
    </article>
  <?php endforeach; ?>
  </section>

  <section class="panel hood">
    <h2>Under the hood</h2>
    <dl>
      <dt>Site</dt><dd><?= $e(getenv('NOVA_SITE')) ?> (<?= $e(getenv('NOVA_MODE')) ?>)</dd>
      <dt>Server</dt><dd><?= $e($_SERVER['SERVER_SOFTWARE'] ?? '?') ?> → <?= $e(PHP_SAPI) ?></dd>
      <dt>Request ID</dt><dd><code><?= $e($_SERVER['NOVA_REQUEST_ID'] ?? '-') ?></code></dd>
      <dt>Protocol</dt><dd><?= $e($_SERVER['SERVER_PROTOCOL']) ?></dd>
      <dt>PHP render</dt><dd><?= number_format((hrtime(true) - $t0) / 1e6, 2) ?> ms</dd>
      <dt>Try</dt><dd><a href="/live.php">NOVA Live demo</a>: partial updates, forms, live data</dd>
      <dt>Endpoints</dt><dd><a href="/_nova/optimize/status">optimizer status</a> · <a href="/_nova/metrics">metrics</a> · <a href="/_nova/health/ready">readiness</a></dd>
    </dl>
  </section>
</main>
<footer>Served by NOVA · <a href="https://www.php.net/">PHP</a> via FastCGI · images by NOVA Optimize</footer>
<script src="/assets/app.js" defer></script>
</body>
</html>
