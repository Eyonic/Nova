<?php
// Files other than the front controller still run in PHP-FPM.
header('Content-Type: text/plain');
echo 'fpm ', PHP_SAPI, "\n";
