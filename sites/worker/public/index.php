<?php
// Front controller placeholder: with `[site.php] worker` set, NOVA sends
// these requests to the booted workers (worker.php) instead of FPM.
http_response_code(500);
echo "worker mode is not enabled for this site\n";
