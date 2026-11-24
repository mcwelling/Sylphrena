//! Images attached to a prompt.
//!
//! A prompt can name image files on the host: `@diagram.png`, a path
//! dragged into the terminal (which may be quoted or have `\ ` escapes), or
//! any word that is the path of an existing image file. Each one is read,
//! checked, and sent with the prompt as an image block; in the text it
//! becomes `[image: name]`.
use std::path::{Path, PathBuf};

use base64::Engine;

use crate::api::types::ImageSource;

/// The API rejects larger images. Base64 adds a third, so this is the
/// limit on the file itself.
pub const MAX_IMAGE_BYTES: usize = 3_750_000;
/// More than this many images in one prompt is almost certainly a mistake.
pub const MAX_IMAGES: usize = 20;
/// All images of one prompt together, so one prompt stays well under the
/// API's 32 MB request limit once base64-encoded.
pub const MAX_TOTAL_IMAGE_BYTES: usize = 15_000_000;

/// A prompt with its images taken out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attached {
    pub text: String,
    pub images: Vec<ImageSource>,
    /// Image paths that were named but couldn't be attached, and why.
    pub problems: Vec<String>,
}

/// The media type of image data, from its first bytes.
pub fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn has_image_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| lower.ends_with(e))
}

/// Words of `text` with their byte spans, honouring '…', "…" and `\ `.
fn words(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut value = String::new();
        let mut end = start;
        let mut quote: Option<char> = None;
        while let Some(&(i, c)) = chars.peek() {
            match (quote, c) {
                (None, c) if c.is_whitespace() => break,
                // A quote opens only at the start of a word: "what's"
                // and "don't" are words, not quotes.
                (None, '\'' | '"') if value.is_empty() && i == start => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, '\\') => {
                    chars.next();
                    end = i + 1;
                    if let Some(&(j, n)) = chars.peek() {
                        value.push(n);
                        end = j + n.len_utf8();
                        chars.next();
                    }
                    continue;
                }
                (_, c) => value.push(c),
            }
            end = i + c.len_utf8();
            chars.next();
        }
        out.push((start, end, value));
    }
    out
}

fn expand(path: &str, base: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

fn load(path: &Path, budget: usize) -> Result<ImageSource, String> {
    // Check the size before reading: the file could be huge.
    let len = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len() as usize;
    if len > MAX_IMAGE_BYTES {
        return Err(format!(
            "{}: {:.1} MB is over the {:.1} MB limit for images",
            path.display(),
            len as f64 / 1e6,
            MAX_IMAGE_BYTES as f64 / 1e6
        ));
    }
    if len > budget {
        return Err(format!(
            "{}: the prompt's images would be over {:.0} MB in all",
            path.display(),
            MAX_TOTAL_IMAGE_BYTES as f64 / 1e6
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let media = sniff_media_type(&bytes)
        .ok_or_else(|| format!("{}: not a PNG, JPEG, GIF or WebP image", path.display()))?;
    Ok(ImageSource::base64(media, base64::engine::general_purpose::STANDARD.encode(&bytes)))
}

/// Find and load the images a prompt names. Paths are relative to `base`.
pub fn attach_images(text: &str, base: &Path) -> Attached {
    let mut result = Attached::default();
    let mut rebuilt = String::new();
    let mut last = 0;
    for (start, mut end, word) in words(text) {
        let (marked, mut candidate) = match word.strip_prefix('@') {
            Some(rest) => (true, rest.to_string()),
            None => (false, word.clone()),
        };
        // "see a.png," or "(b.png)": the punctuation isn't part of the path.
        while candidate.ends_with([',', '.', ';', ':', '!', '?', ')']) && !has_image_extension(&candidate) {
            candidate.pop();
            if text[..end].ends_with([',', '.', ';', ':', '!', '?', ')']) {
                end -= 1;
            }
        }
        if candidate.is_empty() || !has_image_extension(&candidate) {
            continue;
        }
        // Without '@', only a full path counts (what dragging a file into
        // the terminal gives): "rename logo.png" means the name, not the image.
        if !(marked || candidate.starts_with('/') || candidate.starts_with("~/")) {
            continue;
        }
        let path = expand(&candidate, base);
        if !path.is_file() {
            if marked {
                result.problems.push(format!("{}: no such file", path.display()));
            }
            continue;
        }
        if result.images.len() >= MAX_IMAGES {
            result.problems.push(format!("{}: more than {MAX_IMAGES} images; left out", path.display()));
            continue;
        }
        let used: usize = result.images.iter().map(|i| i.data.len() / 4 * 3).sum();
        match load(&path, MAX_TOTAL_IMAGE_BYTES.saturating_sub(used)) {
            Ok(img) => {
                result.images.push(img);
                // Keep the path as written, so the model can still act on the file.
                rebuilt.push_str(&text[last..start]);
                rebuilt.push_str(&format!("[image: {candidate}]"));
                last = end;
            }
            Err(e) => result.problems.push(e),
        }
    }
    rebuilt.push_str(&text[last..]);
    result.text = rebuilt;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];

    #[test]
    fn words_honour_quotes_and_escapes() {
        let w: Vec<String> = words(r#"look at '/a b/c.png' and /x\ y.png now"#).into_iter().map(|w| w.2).collect();
        assert_eq!(w, ["look", "at", "/a b/c.png", "and", "/x y.png", "now"]);
    }

    #[test]
    fn attaches_marked_and_dropped_paths() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("shot 1.png"), PNG).unwrap();
        std::fs::write(tmp.path().join("b.png"), PNG).unwrap();
        std::fs::write(tmp.path().join("fake.png"), b"not an image").unwrap();
        let dropped = tmp.path().join("shot 1.png").display().to_string().replace(' ', "\\ ");
        let a = attach_images(&format!("what's in @b.png and {dropped}? also @missing.png, @fake.png"), tmp.path());
        assert_eq!(a.images.len(), 2, "{:?}", a.problems);
        assert_eq!(a.images[0].media_type, "image/png");
        let full = tmp.path().join("shot 1.png").display().to_string();
        assert_eq!(a.text, format!("what's in [image: b.png] and [image: {full}]? also @missing.png, @fake.png"));
        assert_eq!(a.problems.len(), 2, "{:?}", a.problems);
    }

    #[test]
    fn plain_words_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("logo.png"), PNG).unwrap();
        let a = attach_images("don't rename logo.png to icon.png", tmp.path());
        assert_eq!(a.text, "don't rename logo.png to icon.png");
        assert!(a.images.is_empty() && a.problems.is_empty());
    }
}
