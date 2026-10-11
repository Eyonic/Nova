<?php
header('Content-Type: text/plain');
echo 'user=', $_SERVER['HTTP_REMOTE_USER'] ?? '-';
