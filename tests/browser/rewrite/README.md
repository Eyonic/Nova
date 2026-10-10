# html_rewrite measurement

`gallery.php` is a deliberately naive page (12 full-size `<img>` without
any attributes, 4 photos from the showcase site). `measure.mjs` loads it
in headless Chromium on a throttled Fast 4G connection (desktop 1280x800
and phone 390x844@3x) and prints load time, LCP, CLS and image bytes at
load and after scrolling through the page.

Run a NOVA site with the photos in `public/images/`, `gallery.php` in
`public/`, Optimizer image formats on, and `html_rewrite` false/true; then:

    docker run --rm --network host --init -e URL=http://127.0.0.1:9005/gallery.php \
      -v "$PWD:/pw" -w /pw mcr.microsoft.com/playwright:v1.52.0-noble \
      sh -c 'npm i playwright@1.52.0 >/dev/null && node measure.mjs'
