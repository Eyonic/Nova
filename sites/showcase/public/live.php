<?php
// NOVA Live demo: plain PHP, no framework. Every feature also works without
// JavaScript (links navigate, the form posts) — NOVA Live only enhances it.
session_start();
$_SESSION['csrf'] ??= bin2hex(random_bytes(16));

$isLive = ($_SERVER['HTTP_NOVA_LIVE'] ?? '') === '1';
$target = $_SERVER['HTTP_NOVA_TARGET'] ?? '';
$e = fn($s) => htmlspecialchars((string) $s, ENT_QUOTES);

// --- Notes, stored in this site's private state directory --------------------
$store = dirname(sys_get_temp_dir()) . '/notes.json';
function load_notes(string $store): array {
    $raw = @file_get_contents($store);
    return $raw ? (json_decode($raw, true) ?: []) : [];
}
function save_note(string $store, string $text): void {
    $fh = fopen($store, 'c+');
    flock($fh, LOCK_EX);
    $notes = json_decode(stream_get_contents($fh) ?: '[]', true) ?: [];
    array_unshift($notes, ['text' => $text, 'at' => time()]);
    $notes = array_slice($notes, 0, 8);
    ftruncate($fh, 0);
    rewind($fh);
    fwrite($fh, json_encode($notes));
    flock($fh, LOCK_UN);
    fclose($fh);
}

$errors = [];
$draft = '';
if ($_SERVER['REQUEST_METHOD'] === 'POST') {
    $draft = trim((string) ($_POST['note'] ?? ''));
    if (!hash_equals($_SESSION['csrf'], (string) ($_POST['csrf'] ?? ''))) {
        $errors[] = 'Your session expired. Reload the page and try again.';
    } elseif ($draft === '') {
        $errors[] = 'Write something first.';
    } elseif (mb_strlen($draft) > 140) {
        $errors[] = 'Keep it under 140 characters (' . mb_strlen($draft) . ' now).';
    }
    if ($errors) {
        http_response_code(422); // rendered in place by NOVA Live, or as a normal page
    } else {
        save_note($store, $draft);
        header('Nova-Publish: notes'); // every open page subscribed to "notes" refreshes
        header('Location: /live.php', true, 303); // Post/Redirect/Get
        exit;
    }
}

// --- Products ------------------------------------------------------------------
$products = [
    ['Aurora lamp', 'lighting', 89], ['Dune rug', 'textiles', 249], ['Coral vase', 'decor', 59],
    ['Nebula print', 'decor', 120], ['Ember pendant', 'lighting', 145], ['Tide throw', 'textiles', 79],
];
$categories = ['all', 'lighting', 'textiles', 'decor'];
$category = in_array($_GET['category'] ?? 'all', $categories, true) ? ($_GET['category'] ?? 'all') : 'all';
$shown = array_filter($products, fn($p) => $category === 'all' || $p[1] === $category);

function render_products(array $shown, string $category, callable $e): void { ?>
    <p class="muted">Showing <strong><?= $e($category) ?></strong> · rendered <?= date('H:i:s') ?></p>
    <ul class="products">
      <?php foreach ($shown as [$name, $cat, $price]): ?>
        <li><span><?= $e($name) ?></span><span class="muted"><?= $e($cat) ?></span><span>€<?= $price ?></span></li>
      <?php endforeach; ?>
    </ul>
<?php }

// Server-side optimization (optional): a Live request for the product list
// only needs that fragment. Without this branch NOVA Live extracts the
// region from the full page instead — both work.
if ($isLive && $target === '#products') {
    render_products($shown, $category, $e);
    exit;
}
$notes = load_notes($store);
?>
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>NOVA Live<?= $category !== 'all' ? ' · ' . $e($category) : '' ?></title>
  <link rel="stylesheet" href="/assets/style.css">
  <link rel="stylesheet" href="/assets/live.css">
  <script src="/_nova/live.js" defer></script>
</head>
<body>
<main class="live">
  <p><a href="/">← Gallery</a></p>
  <h1 class="live-title">NOVA Live</h1>
  <p class="lead">Parts of this page update without a reload. It's plain PHP and HTML attributes;
    with JavaScript turned off everything still works as normal links and forms.</p>

  <section class="panel">
    <h2>1 · Filter without reloading</h2>
    <nav class="pills" aria-label="Category">
      <?php foreach ($categories as $c): ?>
        <a href="/live.php?category=<?= $e($c) ?>" data-nova-target="#products"
           <?= $c === $category ? 'aria-current="page"' : '' ?>><?= $e(ucfirst($c)) ?></a>
      <?php endforeach; ?>
    </nav>
    <div id="products" data-nova-live aria-live="polite">
      <?php render_products($shown, $category, $e); ?>
    </div>
    <p class="hint">The URL and Back button follow along. The server returns only the list fragment
      (it checks the <code>Nova-Live</code> header).</p>
  </section>

  <section class="panel">
    <h2>2 · Forms with validation, live across tabs</h2>
    <div id="note-form" data-nova-live>
      <form method="post" action="/live.php" data-nova-target="#note-form" data-nova-reset>
        <input type="hidden" name="csrf" value="<?= $e($_SESSION['csrf']) ?>">
        <label for="note">Leave a note (max 140 characters)</label>
        <div class="row">
          <input id="note" name="note" value="<?= $e($draft) ?>" autocomplete="off"
                 <?= $errors ? 'aria-invalid="true" aria-describedby="note-errors"' : '' ?>>
          <button type="submit">Post</button>
        </div>
        <?php if ($errors): ?>
          <ul id="note-errors" class="errors" role="alert">
            <?php foreach ($errors as $err): ?><li><?= $e($err) ?></li><?php endforeach; ?>
          </ul>
        <?php endif; ?>
      </form>
    </div>
    <div id="notes" data-nova-live data-nova-subscribe="notes" data-nova-src="/live.php" aria-live="polite">
      <?php if (!$notes): ?><p class="muted">No notes yet.</p><?php endif; ?>
      <ul class="notes">
        <?php foreach ($notes as $n): ?>
          <li><?= $e($n['text']) ?> <span class="muted"><?= date('H:i:s', $n['at']) ?></span></li>
        <?php endforeach; ?>
      </ul>
    </div>
    <p class="hint">Open this page in two tabs and post a note: the other tab updates instantly.
      PHP sends <code>Nova-Publish: notes</code>; NOVA tells subscribed pages to refetch
      (no content is pushed, so each viewer only ever sees what PHP renders for them).</p>
  </section>

  <section class="panel">
    <h2>3 · Polling</h2>
    <div id="clock" data-nova-live data-nova-poll="5s" data-nova-src="/live.php">
      <p class="big"><?= date('H:i:s') ?></p>
      <p class="muted">Server time, refreshed every 5 seconds (paused while the tab is hidden).</p>
    </div>
  </section>
</main>
</body>
</html>
