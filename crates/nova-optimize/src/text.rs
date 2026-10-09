//! NOVA Script Optimizer: background minification and precompression of
//! JavaScript, CSS and other text assets.
//!
//! Work happens only when a file changes, never per request:
//!
//! * **Minify** (JS, CSS) with conservative settings: whitespace, comments
//!   (license comments are kept) and local-name mangling for JavaScript;
//!   no top-level renaming, no property mangling, no dead-code removal.
//!   CSS goes through lightningcss without browser targets (no prefix or
//!   syntax lowering). Files that already look minified are left as they are.
//! * **Validate**: the output must parse again, or the original is used.
//! * **Precompress** the result with brotli (11), zstd (19) and gzip (9),
//!   keeping only encodings that actually save bytes.
//! * **Publish** derivation-addressed objects and an atomically replaced
//!   manifest; requests pick the best representation for `Accept-Encoding`.
//! * **Report** sizes, savings, duplicate files and files nothing seems to
//!   reference. Code is never removed based on such observations.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::Path;

pub const TEXT_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextKind {
    Js,
    Css,
    /// Compressed only (SVG, JSON, XML, HTML, source maps, ...).
    Other,
}

impl TextKind {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "js" | "mjs" | "cjs" => TextKind::Js,
            "css" => TextKind::Css,
            "svg" | "json" | "xml" | "txt" | "html" | "htm" | "map" | "webmanifest" | "wasm"
            | "ico" | "rss" | "atom" | "csv" | "md" | "ttf" | "otf" => TextKind::Other,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    Br,
    Zstd,
    Gzip,
}

impl Encoding {
    pub fn token(self) -> &'static str {
        match self {
            Encoding::Br => "br",
            Encoding::Zstd => "zstd",
            Encoding::Gzip => "gzip",
        }
    }

    fn from_token(t: &str) -> Option<Self> {
        match t {
            "br" => Some(Encoding::Br),
            "zstd" => Some(Encoding::Zstd),
            "gzip" => Some(Encoding::Gzip),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextManifest {
    pub version: u32,
    pub site: String,
    pub profile: String,
    pub generated_at: u64,
    pub files: BTreeMap<String, TextAsset>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub errors: BTreeMap<String, crate::manifest::AssetError>,
    #[serde(default)]
    pub report: Report,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextAsset {
    pub kind: TextKind,
    pub hash: String,
    pub bytes: u64,
    pub mtime_ns: u128,
    /// Minified identity representation, when minification helped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minified: Option<Object>,
    /// Precompressed representations of the (minified) content.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub encoded: BTreeMap<Encoding, Object>,
    /// Why the file was not minified (already minified, parse error, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Object {
    pub object: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub files: u64,
    pub original: u64,
    pub minified: u64,
    pub brotli: u64,
}

/// What the optimizer learned about a site's scripts and styles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub js: Totals,
    pub css: Totals,
    /// Files with identical content under different paths.
    pub duplicates: Vec<Vec<String>>,
    /// JS/CSS files whose name appears in no other text file of the project.
    /// Candidates to review, not proof that the code is unused.
    pub unreferenced: Vec<String>,
}

/// Result of processing one file.
pub struct Processed {
    pub asset: TextAsset,
    /// Objects to write: (object path, content).
    pub objects: Vec<(String, Vec<u8>)>,
}

/// Heuristic: long lines and little whitespace mean someone already minified it.
pub fn looks_minified(name: &str, text: &str) -> bool {
    if name.contains(".min.") {
        return true;
    }
    let lines = text.lines().count().max(1);
    let avg = text.len() / lines;
    let ws = text.bytes().filter(|b| *b == b' ' || *b == b'\t').count();
    avg > 300 && ws * 20 < text.len()
}

pub fn minify_js(text: &str) -> Result<String, String> {
    use oxc_allocator::Allocator;
    use oxc_codegen::{Codegen, CodegenOptions, CommentOptions, LegalComment};
    use oxc_minifier::{MangleOptions, MangleOptionsKeepNames, Minifier, MinifierOptions};
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    // Classic scripts first (globals stay globals); ES modules when needed.
    for source_type in [SourceType::script(), SourceType::mjs()] {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, text, source_type).parse();
        if ret.fatal_error || ret.diagnostics.has_errors() {
            continue;
        }
        let mut program = ret.program;
        let options = MinifierOptions {
            mangle: Some(MangleOptions {
                top_level: Some(false),
                keep_names: MangleOptionsKeepNames {
                    function: true,
                    class: true,
                },
                ..MangleOptions::default()
            }),
            mangle_properties: None,
            compress: None,
        };
        let minified = Minifier::new(options).minify(&allocator, &mut program);
        let mut codegen = CodegenOptions::minify();
        codegen.comments = CommentOptions {
            legal: LegalComment::Inline,
            ..CommentOptions::disabled()
        };
        let code = Codegen::new()
            .with_options(codegen)
            .with_scoping(minified.scoping)
            .build(&program)
            .code;
        // Validate: the output must parse as the same kind of program.
        let check = Allocator::default();
        let again = Parser::new(&check, &code, source_type).parse();
        if again.fatal_error || again.diagnostics.has_errors() {
            return Err("minified output did not parse".into());
        }
        return Ok(code);
    }
    Err("not valid JavaScript (script or module)".into())
}

pub fn minify_css(text: &str) -> Result<String, String> {
    use lightningcss::printer::PrinterOptions;
    use lightningcss::stylesheet::{MinifyOptions, ParserOptions, StyleSheet};
    let mut sheet = StyleSheet::parse(text, ParserOptions::default()).map_err(|e| e.to_string())?;
    sheet
        .minify(MinifyOptions::default())
        .map_err(|e| e.to_string())?;
    let out = sheet
        .to_css(PrinterOptions {
            minify: true,
            ..PrinterOptions::default()
        })
        .map_err(|e| e.to_string())?
        .code;
    StyleSheet::parse(&out, ParserOptions::default())
        .map_err(|_| "minified output did not parse".to_string())?;
    Ok(out)
}

/// Files up to this size get the slowest, strongest settings; larger ones
/// (data feeds, source maps) use levels that are ~10x faster for a few
/// percent less savings.
const MAX_EFFORT_BYTES: usize = 512 << 10;

fn compress(data: &[u8], enc: Encoding) -> std::io::Result<Vec<u8>> {
    let max = data.len() <= MAX_EFFORT_BYTES;
    match enc {
        Encoding::Br => {
            let mut out = Vec::new();
            {
                let (q, window) = if max { (11, 22) } else { (9, 24) };
                let mut w = brotli::CompressorWriter::new(&mut out, 64 * 1024, q, window);
                w.write_all(data)?;
            }
            Ok(out)
        }
        Encoding::Zstd => zstd::encode_all(data, if max { 19 } else { 12 }),
        Encoding::Gzip => {
            let level = if max {
                flate2::Compression::best()
            } else {
                flate2::Compression::new(6)
            };
            let mut w = flate2::write::GzEncoder::new(Vec::new(), level);
            w.write_all(data)?;
            w.finish()
        }
    }
}

fn object_name(key: &str, ext: &str) -> String {
    format!("{}/{}.{ext}", &key[..2], &key[..32])
}

/// Minify, validate and precompress one file. Pure: writes nothing.
pub fn process(
    rel: &str,
    kind: TextKind,
    data: &[u8],
    mtime_ns: u128,
    profile: &str,
    minify: bool,
) -> Processed {
    let hash = blake3::hash(data).to_hex().to_string();
    let key = |what: &str| blake3::hash(format!("{hash}|{profile}|{what}").as_bytes()).to_hex();
    let mut objects = Vec::new();
    let mut note = None;
    let mut body: Vec<u8> = data.to_vec();
    let mut minified = None;

    if minify && kind != TextKind::Other {
        match std::str::from_utf8(data) {
            Ok(text) if looks_minified(rel, text) => note = Some("already minified".to_string()),
            Ok(text) => {
                let result = match kind {
                    TextKind::Js => minify_js(text),
                    _ => minify_css(text),
                };
                match result {
                    Ok(out) if out.len() < data.len() => {
                        let ext = if kind == TextKind::Js { "js" } else { "css" };
                        let name = object_name(&key("min"), ext);
                        minified = Some(Object {
                            object: name.clone(),
                            bytes: out.len() as u64,
                        });
                        body = out.into_bytes();
                        objects.push((name, body.clone()));
                    }
                    Ok(_) => note = Some("minification saved nothing".into()),
                    Err(e) => note = Some(format!("not minified: {e}")),
                }
            }
            Err(_) => note = Some("not UTF-8; not minified".into()),
        }
    }

    let mut encoded = BTreeMap::new();
    for enc in [Encoding::Br, Encoding::Zstd, Encoding::Gzip] {
        let Ok(packed) = compress(&body, enc) else {
            continue;
        };
        // Keep only encodings that save at least 5 %.
        if (packed.len() as u64) * 100 < (body.len() as u64) * 95 {
            let name = object_name(&key(enc.token()), enc.token());
            encoded.insert(
                enc,
                Object {
                    object: name.clone(),
                    bytes: packed.len() as u64,
                },
            );
            objects.push((name, packed));
        }
    }

    Processed {
        asset: TextAsset {
            kind,
            hash,
            bytes: data.len() as u64,
            mtime_ns,
            minified,
            encoded,
            note,
        },
        objects,
    }
}

/// A representation chosen for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChoice {
    pub object: String,
    pub encoding: Option<Encoding>,
}

/// Pick the first encoding the client accepts (tokens in preference order),
/// else the minified identity version, else nothing (serve the original).
pub fn choose(asset: &TextAsset, accepted: &[&str]) -> Option<TextChoice> {
    for t in accepted {
        if let Some(enc) = Encoding::from_token(t)
            && let Some(o) = asset.encoded.get(&enc)
        {
            return Some(TextChoice {
                object: o.object.clone(),
                encoding: Some(enc),
            });
        }
    }
    asset.minified.as_ref().map(|o| TextChoice {
        object: o.object.clone(),
        encoding: None,
    })
}

/// Sizes, duplicates and unreferenced files for the report.
pub fn build_report(files: &BTreeMap<String, TextAsset>, project: &Path, root: &Path) -> Report {
    let mut r = Report::default();
    for a in files.values() {
        let t = match a.kind {
            TextKind::Js => &mut r.js,
            TextKind::Css => &mut r.css,
            TextKind::Other => continue,
        };
        t.files += 1;
        t.original += a.bytes;
        let min = a.minified.as_ref().map_or(a.bytes, |m| m.bytes);
        t.minified += min;
        t.brotli += a.encoded.get(&Encoding::Br).map_or(min, |o| o.bytes);
    }

    let mut by_hash: HashMap<&str, Vec<String>> = HashMap::new();
    for (rel, a) in files {
        if a.bytes >= 512 && a.kind != TextKind::Other {
            by_hash.entry(&a.hash).or_default().push(rel.clone());
        }
    }
    r.duplicates = by_hash.into_values().filter(|v| v.len() > 1).collect();
    r.duplicates.sort();

    let names: HashSet<String> = files
        .iter()
        .filter(|(_, a)| a.kind != TextKind::Other)
        .filter_map(|(rel, _)| rel.rsplit('/').next().map(str::to_string))
        .collect();
    let found = referenced_names(project, root, &names);
    r.unreferenced = files
        .iter()
        .filter(|(_, a)| a.kind != TextKind::Other)
        .filter(|(rel, _)| rel.rsplit('/').next().is_some_and(|n| !found.contains(n)))
        .map(|(rel, _)| rel.clone())
        .collect();
    r
}

/// Which of `names` appear in the project's source and markup files.
/// Dependency trees and build caches are skipped; reading stops at 64 MiB.
fn referenced_names(project: &Path, _root: &Path, names: &HashSet<String>) -> HashSet<String> {
    const SKIP: [&str; 6] = ["vendor", "node_modules", "storage", "cache", "tmp", "logs"];
    const EXTS: [&str; 16] = [
        "php",
        "html",
        "htm",
        "twig",
        "js",
        "mjs",
        "cjs",
        "ts",
        "tsx",
        "jsx",
        "vue",
        "svelte",
        "css",
        "json",
        "md",
        "webmanifest",
    ];
    let mut found = HashSet::new();
    let mut budget: u64 = 64 << 20;
    let walker = walkdir::WalkDir::new(project)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e
                    .file_name()
                    .to_str()
                    .is_some_and(|n| (n.starts_with('.') && n != ".vite") || SKIP.contains(&n))
        });
    for entry in walker.flatten() {
        if found.len() == names.len() || budget == 0 {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !EXTS.contains(&ext.as_str()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() > 4 << 20 {
            continue;
        }
        budget = budget.saturating_sub(meta.len());
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let own = entry.file_name().to_str().unwrap_or("");
        for n in names {
            // A file mentioning only itself does not count.
            if n != own && !found.contains(n) && text.contains(n.as_str()) {
                found.insert(n.clone());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = r#"
        /*! Example v1 | MIT */
        // Adds the cart button behaviour.
        var globalCounter = 0;
        function addToCart(productIdentifier, requestedQuantity) {
            var resultingTotal = requestedQuantity * 2;
            globalCounter = globalCounter + resultingTotal;
            return productIdentifier + ':' + resultingTotal;
        }
        window.addToCart = addToCart;
    "#;

    #[test]
    fn js_minification_is_conservative() {
        let out = minify_js(SCRIPT).unwrap();
        assert!(out.len() < SCRIPT.len() / 2, "{out}");
        assert!(out.contains("Example v1 | MIT"), "license kept: {out}");
        assert!(!out.contains("Adds the cart"), "comments dropped");
        // Top-level names stay: other scripts may use them.
        assert!(out.contains("globalCounter") && out.contains("function addToCart"));
        // Locals are mangled.
        assert!(!out.contains("productIdentifier"));
    }

    #[test]
    fn modules_and_invalid_input() {
        let out = minify_js("import { a } from './a.js';\nexport const value = a + 1;\n").unwrap();
        assert!(out.contains("import"));
        assert!(minify_js("function (").is_err());
    }

    #[test]
    fn css_minification() {
        let out =
            minify_css("a  {\n  color : #ff0000 ;\n}\n\n/* note */\nb { margin: 0px }\n").unwrap();
        assert!(out.len() < 30, "{out}");
        assert!(minify_css("a { color: red").is_ok()); // CSS error recovery is lenient by spec
    }

    #[test]
    fn process_and_choose() {
        let p = process(
            "app.js",
            TextKind::Js,
            SCRIPT.repeat(20).as_bytes(),
            1,
            "p",
            true,
        );
        let a = &p.asset;
        assert!(a.minified.is_some());
        assert!(a.encoded.contains_key(&Encoding::Br));
        assert_eq!(p.objects.len(), 1 + a.encoded.len());
        let c = choose(a, &["zstd", "br"]).unwrap();
        assert_eq!(c.encoding, Some(Encoding::Zstd));
        let identity = choose(a, &[]).unwrap();
        assert_eq!(identity.encoding, None);
        assert_eq!(identity.object, a.minified.as_ref().unwrap().object);

        let already = process("x.min.js", TextKind::Js, SCRIPT.as_bytes(), 1, "p", true);
        assert_eq!(already.asset.note.as_deref(), Some("already minified"));
        assert!(already.asset.minified.is_none());
    }

    #[test]
    fn report_finds_duplicates_and_unreferenced() {
        let dir = std::env::temp_dir().join(format!("nova-text-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("public/js")).unwrap();
        std::fs::write(
            dir.join("public/index.html"),
            "<script src=/js/used.js></script>",
        )
        .unwrap();
        let mut files = BTreeMap::new();
        for name in ["js/used.js", "js/orphan.js", "js/copy.js"] {
            let data = if name == "js/used.js" {
                "a".repeat(600)
            } else {
                "b".repeat(600)
            };
            files.insert(
                name.to_string(),
                process(name, TextKind::Js, data.as_bytes(), 1, "p", false).asset,
            );
        }
        let r = build_report(&files, &dir, &dir.join("public"));
        assert_eq!(r.js.files, 3);
        assert_eq!(
            r.duplicates,
            vec![vec!["js/copy.js".to_string(), "js/orphan.js".to_string()]]
        );
        assert_eq!(
            r.unreferenced,
            vec!["js/copy.js".to_string(), "js/orphan.js".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
