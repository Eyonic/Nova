<?php
// Long-running request, used to verify graceful shutdown drains in-flight work.
sleep(min((int) ($_GET['s'] ?? 2), 10));
echo "done\n";
