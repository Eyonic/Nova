# NOVA vs nginx + PHP-FPM vs FrankenPHP

Head-to-head on a real server (Unraid, Ryzen 9 5900X), next to NOVA's
`nova-exp` test stack (WordPress 7.1 + MariaDB, Laravel 13 + Octane):

* every server pinned to the same 4 cores (4-7), one at a time under load;
* load from `oha` in a container on the same Docker network (no userland
  proxy), on other cores; a browser's `Accept-Encoding`;
* same PHP settings everywhere (`php.ini`: OPcache + tracing JIT, 8 PHP
  processes), same Laravel environment (production, file sessions);
* FrankenPHP built with the PHP extensions WordPress needs (`fp.Dockerfile`:
  the official image has no mysqli);
* 3 interleaved rounds, median reported.

    run.sh setup && run.sh check && run.sh bench html jpg wp up lhome; run.sh teardown
    # HTTPS (HTTP/2, HTTP/3): certificate in certs/ (openssl, P-256), then
    NGINXCONF=nginx-tls.conf CADDYFILE=Caddyfile-tls run.sh setup
    run-tls.sh h2 html jpg wp up; run-tls.sh h3 html wp

Results: `results-2026-10-11-unraid.txt` (HTTP/1.1), `results-2026-10-11-unraid-https.txt` (h2, h3); summary in
docs/reports/2026-10-11-where-nova-stands.md. NOVA ran with
`isolation.php_open_basedir = "never"`, what `auto` picks on hosts with
Landlock or with one PHP site; with `open_basedir` (several PHP sites and
no Landlock) WordPress drops to 85 req/s, exactly as nginx + PHP-FPM does
with `open_basedir` (measured: 113 -> 85).
