//! Streaming HTML rewriting for `<img>` (prototype, `html_rewrite = true`).
//!
//! Adds what a hand-tuned page would have, and only where the developer
//! did not set it already (their markup always wins):
//!
//! * `loading="lazy"` on every image after the first [`EAGER`] ones,
//! * `decoding="async"`,
//! * `fetchpriority="high"` on the first image (likely the LCP element),
//! * `width` / `height` from the Optimizer's metadata (no layout shift),
//! * `srcset` over the Optimizer's responsive widths, with
//!   `sizes="auto, 100vw"` for lazy images (`100vw` for eager ones).
//!
//! `data-nova-keep` on an image leaves it untouched. The document is
//! streamed through `lol_html`; nothing is buffered.

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, BodyStream, StreamBody};
use hyper::body::Frame;
use lol_html::send::HtmlRewriter;
use lol_html::{Settings, element};
use nova_http::Body;
use std::sync::{Arc, Mutex};

/// Images loaded eagerly at the top of the page.
pub const EAGER: usize = 2;

/// What the Optimizer knows about one image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    /// Responsive widths available below the source width.
    pub widths: Vec<u32>,
}

/// Speculation Rules injected with `speculation_rules = true`: prefetch
/// same-site links when the visitor is about to click them (hover or
/// pointer-down; "moderate" eagerness), never for links that change state
/// or need a login, and never for links marked `rel=nofollow` or
/// `data-nova-noprefetch`.
pub const SPECULATION_RULES: &str = r#"<script type="speculationrules">{"prefetch":[{"where":{"and":[{"href_matches":"/*"},{"not":{"href_matches":["/wp-admin/*","/wp-login.php*","/admin/*","/login*","/logout*","/*/logout*","/api/*","/cart*","/checkout*","/*?*action=*","/*?*add-to-cart=*"]}},{"not":{"selector_matches":"[rel~=nofollow], [data-nova-noprefetch], [download], [target=_blank]"}}]},"eagerness":"moderate"}]}</script>"#;

/// What to rewrite.
pub struct Options {
    /// `<img>` attributes (`html_rewrite`).
    pub images: bool,
    /// Append [`SPECULATION_RULES`] before `</body>` unless the page has
    /// its own `<script type="speculationrules">` (`speculation_rules`).
    pub speculation: bool,
}

/// Rewrite an HTML body. `lookup` maps an image URL path (`/images/a.jpg`)
/// to its metadata; `base` is the request path, for relative `src`.
pub fn rewrite(
    body: Body,
    base: String,
    opts: Options,
    lookup: impl Fn(&str) -> Option<ImageInfo> + Send + 'static,
) -> Body {
    let out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&out);
    let mut index = 0usize;
    let has_rules = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handlers = Vec::new();
    if opts.speculation {
        let seen = Arc::clone(&has_rules);
        handlers.push(element!("script[type=speculationrules]", move |_el| {
            seen.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }));
        let seen = Arc::clone(&has_rules);
        handlers.push(element!("body", move |el| {
            let seen = Arc::clone(&seen);
            type EndTagHandler = Box<
                dyn FnOnce(&mut lol_html::html_content::EndTag<'_>) -> lol_html::HandlerResult
                    + Send,
            >;
            let insert: EndTagHandler = Box::new(move |end| {
                if !seen.load(std::sync::atomic::Ordering::Relaxed) {
                    end.before(SPECULATION_RULES, lol_html::html_content::ContentType::Html);
                }
                Ok(())
            });
            if let Some(handlers) = el.end_tag_handlers() {
                handlers.push(insert);
            }
            Ok(())
        }));
    }
    if opts.images {
        handlers.push(element!("img", move |el| {
            let i = index;
            index += 1;
            if el.has_attribute("data-nova-keep") {
                return Ok(());
            }
            let eager = i < EAGER;
            if !el.has_attribute("loading") && !eager {
                el.set_attribute("loading", "lazy")?;
            }
            if !el.has_attribute("decoding") {
                el.set_attribute("decoding", "async")?;
            }
            if i == 0 && !el.has_attribute("fetchpriority") {
                el.set_attribute("fetchpriority", "high")?;
            }
            let Some(src) = el.get_attribute("src") else {
                return Ok(());
            };
            let Some(path) = local_path(&src, &base) else {
                return Ok(());
            };
            let Some(info) = lookup(&path) else {
                return Ok(());
            };
            if !el.has_attribute("width") && !el.has_attribute("height") {
                el.set_attribute("width", &info.width.to_string())?;
                el.set_attribute("height", &info.height.to_string())?;
            }
            if !el.has_attribute("srcset") && !src.contains('?') && !info.widths.is_empty() {
                let mut set: Vec<String> = info
                    .widths
                    .iter()
                    .map(|w| format!("{src}?w={w} {w}w"))
                    .collect();
                set.push(format!("{src} {}w", info.width));
                el.set_attribute("srcset", &set.join(", "))?;
                if !el.has_attribute("sizes") {
                    let lazy = el.get_attribute("loading").as_deref() == Some("lazy");
                    el.set_attribute("sizes", if lazy { "auto, 100vw" } else { "100vw" })?;
                }
            }
            Ok(())
        }));
    }
    let rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: handlers,
            ..Settings::new_send()
        },
        move |chunk: &[u8]| sink.lock().unwrap().extend_from_slice(chunk),
    );
    let take = move || Bytes::from(std::mem::take(&mut *out.lock().unwrap()));
    let stream = futures_util::stream::unfold(
        (BodyStream::new(body), Some(rewriter), take),
        |(mut body, mut rewriter, take)| async move {
            let rw = rewriter.as_mut()?;
            loop {
                match body.next().await {
                    Some(Ok(frame)) => {
                        let Ok(data) = frame.into_data() else {
                            continue;
                        };
                        if let Err(e) = rw.write(&data) {
                            return Some((Err(std::io::Error::other(e)), (body, None, take)));
                        }
                        let produced = take();
                        if !produced.is_empty() {
                            return Some((Ok(produced), (body, rewriter, take)));
                        }
                    }
                    Some(Err(e)) => return Some((Err(e), (body, None, take))),
                    None => {
                        let rw = rewriter.take()?;
                        if let Err(e) = rw.end() {
                            return Some((Err(std::io::Error::other(e)), (body, None, take)));
                        }
                        return Some((Ok(take()), (body, None, take)));
                    }
                }
            }
        },
    );
    StreamBody::new(stream.map(|r| r.map(Frame::data))).boxed_unsync()
}

/// URL path of a same-site image `src`, resolved against `base`.
fn local_path(src: &str, base: &str) -> Option<String> {
    let src = src.trim();
    if src.is_empty() || src.contains("://") || src.starts_with("//") || src.starts_with("data:") {
        return None;
    }
    let path = src.split(['?', '#']).next()?;
    if path.starts_with('/') {
        return Some(path.to_string());
    }
    let dir = &base[..base.rfind('/').map_or(0, |i| i + 1)];
    Some(format!("{}{path}", if dir.is_empty() { "/" } else { dir }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_http::full;

    async fn run(html: &'static str) -> String {
        let lookup = |p: &str| {
            (p == "/images/a.jpg").then(|| ImageInfo {
                width: 1600,
                height: 900,
                widths: vec![320, 640],
            })
        };
        let opts = Options {
            images: true,
            speculation: true,
        };
        let body = rewrite(full(html), "/blog/post".into(), opts, lookup);
        String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn adds_loading_dimensions_and_srcset() {
        let out = run(r#"<p><img src="/images/a.jpg"><img src="../images/a.jpg"><img src="/images/a.jpg" alt=x></p>"#).await;
        let imgs: Vec<&str> = out.split("<img").skip(1).collect();
        assert!(
            imgs[0].contains(r#"fetchpriority="high""#) && !imgs[0].contains("loading="),
            "{out}"
        );
        assert!(imgs[0].contains(r#"width="1600" height="900""#), "{out}");
        assert!(imgs[0].contains(r#"srcset="/images/a.jpg?w=320 320w, /images/a.jpg?w=640 640w, /images/a.jpg 1600w""#), "{out}");
        assert!(imgs[0].contains(r#"sizes="100vw""#), "{out}");
        assert!(
            !imgs[1].contains("width="),
            "relative path resolves to /blog/../images: unknown"
        );
        assert!(
            imgs[2].contains(r#"loading="lazy""#) && imgs[2].contains(r#"sizes="auto, 100vw""#),
            "{out}"
        );
        assert!(imgs.iter().all(|i| i.contains(r#"decoding="async""#)));
    }

    #[tokio::test]
    async fn developer_markup_wins() {
        let out = run(r#"<img src="/images/a.jpg" loading="eager" width="10" height="5" srcset="x 1w" decoding="sync"><img src="/a.jpg" data-nova-keep><img src="https://cdn.example/a.jpg">"#).await;
        assert!(
            out.contains(r#"loading="eager""#)
                && out.contains(r#"width="10""#)
                && out.contains(r#"srcset="x 1w""#)
                && out.contains(r#"decoding="sync""#),
            "{out}"
        );
        assert!(
            out.contains(r#"<img src="/a.jpg" data-nova-keep>"#),
            "{out}"
        );
        assert!(!out.contains("cdn.example/a.jpg?w="), "{out}");
    }

    #[test]
    fn paths() {
        assert_eq!(
            local_path("/i/a.jpg?v=1", "/x").as_deref(),
            Some("/i/a.jpg")
        );
        assert_eq!(
            local_path("a.jpg", "/blog/post").as_deref(),
            Some("/blog/a.jpg")
        );
        assert_eq!(local_path("a.jpg", "/").as_deref(), Some("/a.jpg"));
        assert_eq!(local_path("https://x/a.jpg", "/"), None);
        assert_eq!(local_path("//x/a.jpg", "/"), None);
        assert_eq!(local_path("data:image/png;base64,AA", "/"), None);
    }

    #[tokio::test]
    async fn speculation_rules_once_and_respecting_the_page() {
        let out = run("<html><body><a href=/a>a</a></body></html>").await;
        assert_eq!(out.matches("speculationrules").count(), 1, "{out}");
        assert!(
            out.find("speculationrules").unwrap() < out.find("</body>").unwrap(),
            "{out}"
        );
        let own =
            run(r#"<html><body><script type="speculationrules">{}</script></body></html>"#).await;
        assert_eq!(
            own.matches("speculationrules").count(),
            1,
            "the page's own rules win: {own}"
        );
        let json = SPECULATION_RULES
            .trim_start_matches(r#"<script type="speculationrules">"#)
            .trim_end_matches("</script>");
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["prefetch"][0]["eagerness"], "moderate");
    }
}
