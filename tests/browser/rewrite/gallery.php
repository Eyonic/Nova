<?php
// A naively written page: full-size images, no loading/size/srcset attributes.
$imgs = ['aurora', 'coral', 'dunes', 'nebula'];
?><!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Gallery</title><style>body{font:16px system-ui;max-width:900px;margin:auto;padding:1rem} img{max-width:100%;display:block;margin:1rem 0}</style></head>
<body><h1>Travel gallery</h1><p>Our favourite places, in full resolution.</p>
<?php for ($r = 0; $r < 3; $r++) foreach ($imgs as $i): ?>
<h2><?= ucfirst($i) ?> #<?= $r + 1 ?></h2><img src="/images/<?= $i ?>.jpg" alt="<?= $i ?>">
<p>Some text about <?= $i ?> to read while scrolling.</p>
<?php endforeach; ?>
</body></html>
