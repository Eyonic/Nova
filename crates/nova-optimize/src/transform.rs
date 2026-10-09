//! Analyze → Plan → Transform → Validate for a single source image.
//! Runs on a blocking thread; all codec work goes through established
//! libraries (image, libwebp, rav1e via ravif).

use crate::manifest::{Format, Source};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use nova_config::{ImageFormat, OptimizeConfig};
use std::io::{BufReader, Cursor};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum TransformError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(#[from] image::ImageError),
    #[error("image has {0} pixels, above the configured limit")]
    TooLarge(u64),
    #[error("animated images are not optimized yet")]
    Animated,
    #[error("encode {0:?}: {1}")]
    Encode(Format, String),
    #[error("validation failed for {0:?} {1}px: {2}")]
    Invalid(Format, u32, String),
}

/// What the source looks like (step 2: Analyze).
pub struct Analyzed {
    pub image: DynamicImage,
    pub width: u32,
    pub height: u32,
    pub alpha: bool,
}

pub fn analyze(
    path: &Path,
    format: Format,
    cfg: &OptimizeConfig,
) -> Result<Analyzed, TransformError> {
    let (w, h) = ImageReader::open(path)?
        .with_guessed_format()?
        .into_dimensions()?;
    let pixels = w as u64 * h as u64;
    if pixels > cfg.max_pixels {
        return Err(TransformError::TooLarge(pixels));
    }
    let animated = match format {
        Format::Png => {
            image::codecs::png::PngDecoder::new(BufReader::new(std::fs::File::open(path)?))?
                .is_apng()?
        }
        Format::Webp => {
            image::codecs::webp::WebPDecoder::new(BufReader::new(std::fs::File::open(path)?))?
                .has_animation()
        }
        _ => false,
    };
    if animated {
        return Err(TransformError::Animated);
    }
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(pixels.saturating_mul(16).max(64 << 20));
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let alpha = image.color().has_alpha() && has_transparent_pixel(&image);
    let (width, height) = (image.width(), image.height());
    Ok(Analyzed {
        image,
        width,
        height,
        alpha,
    })
}

fn has_transparent_pixel(img: &DynamicImage) -> bool {
    img.to_rgba8().pixels().any(|p| p.0[3] != 255)
}

/// One output to produce (step 3: Plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Planned {
    pub width: u32,
    pub height: u32,
    pub format: Format,
}

/// Widths below the source plus the source width itself; modern formats at
/// every width, the source format only where it is actually resized.
/// Sources may be JPEG, PNG or (non-animated) WebP.
pub fn plan(src: &Source, cfg: &OptimizeConfig) -> Vec<Planned> {
    let mut widths: Vec<u32> = cfg
        .widths
        .iter()
        .copied()
        .filter(|&w| w < src.width)
        .collect();
    widths.sort_unstable();
    widths.dedup();
    widths.push(src.width);
    let mut out = Vec::new();
    for &w in &widths {
        let h = ((src.height as u64 * w as u64 + src.width as u64 / 2) / src.width as u64).max(1)
            as u32;
        for f in &cfg.formats {
            let format = match f {
                ImageFormat::Avif => Format::Avif,
                ImageFormat::Webp => Format::Webp,
            };
            out.push(Planned {
                width: w,
                height: h,
                format,
            });
        }
        // Source format at resized widths, unless already planned (WebP sources).
        if w < src.width && !out.iter().any(|p| p.width == w && p.format == src.format) {
            out.push(Planned {
                width: w,
                height: h,
                format: src.format,
            });
        }
    }
    out
}

/// Step 4 (Transform) and step 5 (Validate) for one planned output.
pub fn render(
    img: &DynamicImage,
    alpha: bool,
    p: Planned,
    cfg: &OptimizeConfig,
) -> Result<Vec<u8>, TransformError> {
    let resized;
    let img = if p.width == img.width() {
        img
    } else {
        resized = img.resize_exact(p.width, p.height, image::imageops::FilterType::Lanczos3);
        &resized
    };
    let q = &cfg.quality;
    let enc_err = |e: &dyn std::fmt::Display| TransformError::Encode(p.format, e.to_string());
    let bytes = match p.format {
        Format::Avif => {
            let enc = ravif::Encoder::new()
                .with_quality(q.avif)
                .with_alpha_quality(q.avif)
                .with_speed(cfg.avif_speed)
                .with_num_threads(Some(1));
            let (w, h) = (p.width as usize, p.height as usize);
            let out = if alpha {
                let buf = img.to_rgba8();
                let px: &[ravif::RGBA8] = rgb::FromSlice::as_rgba(buf.as_raw().as_slice());
                enc.encode_rgba(ravif::Img::new(px, w, h))
            } else {
                let buf = img.to_rgb8();
                let px: &[ravif::RGB8] = rgb::FromSlice::as_rgb(buf.as_raw().as_slice());
                enc.encode_rgb(ravif::Img::new(px, w, h))
            };
            out.map_err(|e| enc_err(&e))?.avif_file
        }
        Format::Webp => {
            let mem = if alpha {
                let buf = img.to_rgba8();
                webp::Encoder::from_rgba(buf.as_raw(), p.width, p.height).encode(q.webp)
            } else {
                let buf = img.to_rgb8();
                webp::Encoder::from_rgb(buf.as_raw(), p.width, p.height).encode(q.webp)
            };
            mem.to_vec()
        }
        Format::Jpeg => {
            let mut out = Vec::new();
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q.jpeg);
            img.to_rgb8()
                .write_with_encoder(enc)
                .map_err(|e| enc_err(&e))?;
            out
        }
        Format::Png => {
            let mut out = Vec::new();
            let enc = image::codecs::png::PngEncoder::new_with_quality(
                &mut out,
                image::codecs::png::CompressionType::Best,
                image::codecs::png::FilterType::Adaptive,
            );
            if alpha {
                img.to_rgba8().write_with_encoder(enc)
            } else {
                img.to_rgb8().write_with_encoder(enc)
            }
            .map_err(|e| enc_err(&e))?;
            out
        }
    };
    validate(&bytes, p)?;
    Ok(bytes)
}

/// Check that an encoded output is what the plan asked for.
pub fn validate(bytes: &[u8], p: Planned) -> Result<(), TransformError> {
    let invalid = |msg: String| TransformError::Invalid(p.format, p.width, msg);
    if bytes.is_empty() {
        return Err(invalid("empty output".into()));
    }
    if p.format == Format::Avif {
        // No AVIF decoder is linked; verify the ISO-BMFF brand instead.
        if bytes.len() < 12
            || &bytes[4..8] != b"ftyp"
            || !(&bytes[8..12] == b"avif" || &bytes[8..12] == b"avis")
        {
            return Err(invalid("missing AVIF ftyp brand".into()));
        }
        return Ok(());
    }
    let (w, h) = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| invalid(e.to_string()))?
        .into_dimensions()
        .map_err(|e| invalid(e.to_string()))?;
    if (w, h) != (p.width, p.height) {
        return Err(invalid(format!(
            "got {w}x{h}, expected {}x{}",
            p.width, p.height
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(w: u32, h: u32, format: Format) -> Source {
        Source {
            hash: String::new(),
            bytes: 0,
            mtime_ns: 0,
            width: w,
            height: h,
            format,
            alpha: false,
        }
    }

    #[test]
    fn plan_never_upscales() {
        let cfg = OptimizeConfig {
            widths: vec![320, 640, 4000],
            ..Default::default()
        };
        let p = plan(&source(1000, 500, Format::Jpeg), &cfg);
        let widths: Vec<u32> = p.iter().map(|v| v.width).collect();
        assert!(widths.iter().all(|&w| w <= 1000));
        // 320 and 640: avif, webp, jpeg. 1000 (original): avif, webp only.
        assert_eq!(p.len(), 3 + 3 + 2);
        assert!(p.contains(&Planned {
            width: 640,
            height: 320,
            format: Format::Jpeg
        }));
        assert!(!p.contains(&Planned {
            width: 1000,
            height: 500,
            format: Format::Jpeg
        }));
    }

    #[test]
    fn webp_source_plans_no_duplicates() {
        let cfg = OptimizeConfig {
            widths: vec![320],
            ..Default::default()
        };
        let p = plan(&source(1000, 500, Format::Webp), &cfg);
        // 320: avif + webp (webp doubles as the source format); 1000: avif + webp.
        assert_eq!(p.len(), 4, "{p:?}");
    }

    #[test]
    fn analyzes_webp_source() {
        let dir = std::env::temp_dir().join(format!("nova-webp-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.webp");
        let rgb = image::RgbImage::from_fn(40, 20, |x, _| image::Rgb([x as u8 * 6, 80, 160]));
        std::fs::write(
            &path,
            &*webp::Encoder::from_rgb(rgb.as_raw(), 40, 20).encode(80.0),
        )
        .unwrap();
        let a = analyze(&path, Format::Webp, &OptimizeConfig::default()).unwrap();
        assert_eq!((a.width, a.height, a.alpha), (40, 20, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_and_validates_every_format() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(64, 48, |x, y| {
            image::Rgb([x as u8 * 4, y as u8 * 5, 128])
        }));
        let cfg = OptimizeConfig {
            avif_speed: 10,
            ..Default::default()
        };
        for format in [Format::Avif, Format::Webp, Format::Jpeg, Format::Png] {
            let p = Planned {
                width: 32,
                height: 24,
                format,
            };
            let out = render(&img, false, p, &cfg).unwrap();
            assert!(!out.is_empty(), "{format:?}");
        }
    }

    #[test]
    fn validation_rejects_wrong_dimensions() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::new(16, 16));
        let cfg = OptimizeConfig::default();
        let bytes = render(
            &img,
            false,
            Planned {
                width: 16,
                height: 16,
                format: Format::Png,
            },
            &cfg,
        )
        .unwrap();
        assert!(
            validate(
                &bytes,
                Planned {
                    width: 8,
                    height: 8,
                    format: Format::Png
                }
            )
            .is_err()
        );
    }
}
