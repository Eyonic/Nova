//! Step 7 (Serve): choose the best representation for a request.

use crate::manifest::{Asset, Format};

/// Formats a client accepts, from its `Accept` header. Explicit `q=0` excludes.
pub fn accepted(accept: Option<&str>) -> (bool, bool) {
    let (mut avif, mut webp) = (false, false);
    for part in accept.unwrap_or("").split(',') {
        let mut it = part.split(';');
        let media = it.next().unwrap_or("").trim().to_ascii_lowercase();
        let zero = it.any(|p| {
            let p = p.trim();
            p.strip_prefix("q=")
                .is_some_and(|q| q.trim().parse::<f32>().is_ok_and(|q| q <= 0.0))
        });
        if zero {
            continue;
        }
        match media.as_str() {
            "image/avif" => avif = true,
            "image/webp" => webp = true,
            _ => {}
        }
    }
    (avif, webp)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Serve the untouched source file.
    Original,
    /// Serve this object from the store.
    Variant {
        object: String,
        format: Format,
        bytes: u64,
    },
}

/// Pick the smallest acceptable file at the smallest width that covers
/// `want_width` (or the full width when no width is requested).
pub fn choose(asset: &Asset, accept: Option<&str>, want_width: Option<u32>) -> Choice {
    let (avif, webp) = accepted(accept);
    let src = &asset.source;
    let target = match want_width {
        None => src.width,
        Some(w) => {
            let mut widths: Vec<u32> = asset.variants.iter().map(|v| v.width).collect();
            widths.sort_unstable();
            widths.into_iter().find(|&x| x >= w).unwrap_or(src.width)
        }
    };
    let ok = |f: Format| match f {
        Format::Avif => avif,
        Format::Webp => webp,
        f => f == src.format,
    };
    let best = asset
        .variants
        .iter()
        .filter(|v| v.width == target && ok(v.format))
        .min_by_key(|v| v.bytes);
    match best {
        // At full width the original is a candidate too; never serve something bigger.
        Some(v) if target == src.width && v.bytes >= src.bytes => Choice::Original,
        Some(v) => Choice::Variant {
            object: v.object.clone(),
            format: v.format,
            bytes: v.bytes,
        },
        None => Choice::Original,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Source, Variant};

    fn asset() -> Asset {
        let v = |width, format, bytes| Variant {
            width,
            height: width / 2,
            format,
            object: format!("{width}.{format:?}"),
            bytes,
        };
        Asset {
            source: Source {
                hash: "h".into(),
                bytes: 100_000,
                mtime_ns: 1,
                width: 1000,
                height: 500,
                format: Format::Jpeg,
                alpha: false,
            },
            variants: vec![
                v(320, Format::Avif, 3_000),
                v(320, Format::Webp, 4_000),
                v(320, Format::Jpeg, 9_000),
                v(1000, Format::Avif, 20_000),
                v(1000, Format::Webp, 150_000),
            ],
        }
    }

    #[test]
    fn parses_accept() {
        assert_eq!(
            accepted(Some("image/avif,image/webp,*/*;q=0.8")),
            (true, true)
        );
        assert_eq!(accepted(Some("image/webp;q=0, image/avif")), (true, false));
        assert_eq!(accepted(None), (false, false));
    }

    #[test]
    fn picks_smallest_accepted_format() {
        let a = asset();
        assert!(matches!(
            choose(&a, Some("image/avif,image/webp"), None),
            Choice::Variant {
                format: Format::Avif,
                ..
            }
        ));
        // WebP at full width is larger than the source: keep the original.
        assert_eq!(choose(&a, Some("image/webp"), None), Choice::Original);
        assert_eq!(choose(&a, Some("text/html"), None), Choice::Original);
    }

    #[test]
    fn width_selection_rounds_up() {
        let a = asset();
        assert!(matches!(
            choose(&a, Some("image/webp"), Some(200)),
            Choice::Variant {
                format: Format::Webp,
                bytes: 4_000,
                ..
            }
        ));
        assert!(matches!(
            choose(&a, None, Some(300)),
            Choice::Variant {
                format: Format::Jpeg,
                ..
            }
        ));
        // Wider than any variant: full width.
        assert!(matches!(
            choose(&a, Some("image/avif"), Some(5000)),
            Choice::Variant {
                format: Format::Avif,
                bytes: 20_000,
                ..
            }
        ));
    }
}
