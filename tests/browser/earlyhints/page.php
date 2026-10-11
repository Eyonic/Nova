<?php
// A page whose server work takes 200 ms (database, templates, APIs) and
// that needs a stylesheet and a script to render: the case 103 Early Hints
// is for. NOVA learns /eh/app.css and /eh/app.js from this <head>.
usleep(200000);
?><!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Early Hints test</title>
<link rel="stylesheet" href="/eh/app.css">
<script src="/eh/app.js" defer></script>
</head>
<body>
<h1 class="hero">Rendered after the stylesheet</h1>
<p><?= str_repeat('Some page text. ', 200) ?></p>
</body>
</html>
