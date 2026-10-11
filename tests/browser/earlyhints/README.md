# Early Hints measurement

`page.php` takes 200 ms of server time and needs `/eh/app.css` and
`/eh/app.js`; `run.sh` serves it from NOVA over HTTPS with `early_hints`
off and on and loads it in headless Chromium on a throttled connection
(80 ms latency, 20 Mbit/s; `LATENCY=` to change), five fresh visits each,
after one warm-up visit that lets NOVA learn the page.

    tests/browser/earlyhints/run.sh            # image nova:dev
    NOVA_BIN=target/release/nova tests/browser/earlyhints/run.sh
