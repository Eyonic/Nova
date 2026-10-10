# Compression dictionary measurement (RFC 9842)

Simulates a deploy in one Chromium session: load a page with a
fingerprinted bundle (`app-AAAA1111.js`, listed in a Vite manifest so NOVA
serves it as immutable), write `app-BBBB2222.js` (v1 plus a small change)
and point the manifest at it, wait until NOVA's Optimizer has precompressed
it (a `Content-Length` on the brotli response), reload, and report each
bundle's bytes on the wire, its `Content-Encoding`, whether the page runs
the new version (`window.__appVersion`) and any page errors.

Setup: a NOVA site with `index.php` from here in `public/`, a large
minified ES module as `public/build/assets/app-AAAA1111.js`, and
`public/build/manifest.json` = `{"src/main.js":{"file":"assets/app-AAAA1111.js","isEntry":true}}`.
Mount `public/` at `/site` (read-write) in the Playwright container:

    docker run --rm --network host --init -e URL=http://127.0.0.1:9005 \
      -v "$PWD:/pw" -v "$SITE/public:/site" -w /pw mcr.microsoft.com/playwright:v1.52.0-noble \
      sh -c 'npm i playwright@1.52.0 >/dev/null && node measure.mjs'

Result (Chromium 136, 275 KB library): v2 as `dcz` 0.6 KB instead of
52.7 KB brotli; the page runs v2 without errors.
