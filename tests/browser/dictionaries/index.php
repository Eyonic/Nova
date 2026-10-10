<?php $m = json_decode(file_get_contents(__DIR__ . '/build/manifest.json'), true); ?>
<!doctype html><html><head><meta charset="utf-8"><title>app</title>
<script type="module" src="/build/<?= $m['src/main.js']['file'] ?>"></script></head><body>app</body></html>
