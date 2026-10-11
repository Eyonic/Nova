<?php
header('Content-Type: text/plain');
echo 'q=', $_GET['q'] ?? '';
