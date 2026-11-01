//! Images from the agent's machine, shown in a briefing.
//!
//! An agent puts a local file in any Markdown field as an ordinary image, `![alt](/abs/shot.png)`.
//! The process that reads the presentation on the agent's machine (the CLI, or the local
//! `briefing mcp` server) loads each file, rewrites its reference to `briefing-image:<name>`,
//! and sends the bytes along with the presentation. The hub keeps them beside the briefing's
//! record, for as long as the record, and serves them to the page at
//! `/briefing/<id>/images/<name>`. Remote `https:` URLs and `data:` images are left as they are.
//!
//! The hub's own `/mcp` endpoint runs on the hub, not the agent's machine, so it never reads
//! local paths: that would expose the hub's files to a remote client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use pulldown_cmark::{Event, Parser, Tag};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::content::{Briefing, ValidationError};

/// What an uploaded image's reference looks like in the stored presentation.
pub const SCHEME: &str = "briefing-image:";
pub const MAX_IMAGES: usize = 20;
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_TOTAL_IMAGE_BYTES: usize = 25 * 1024 * 1024;

/// One image sent with a presentation, named by its position (`1`, `2`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Image {
    pub name: String,
    /// Base64 on the wire.
    #[serde(serialize_with = "to_base64", deserialize_with = "from_base64")]
    pub data: Vec<u8>,
}

fn to_base64<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(data))
}

fn from_base64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(deserializer)?;
    base64::engine::general_purpose::STANDARD.decode(text).map_err(serde::de::Error::custom)
}

/// The image's media type, from its leading bytes. SVG is deliberately absent: it can carry
/// script.
pub fn media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// A name the hub accepts for an image, which is also safe as a file name.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 3 && name.bytes().all(|b| b.is_ascii_digit())
}

/// Where local image paths come from.
#[derive(Debug, Clone)]
pub enum LocalFiles {
    /// Read them, resolving relative paths against this directory.
    From(PathBuf),
    /// Refuse them: this process does not run on the agent's machine.
    Refused,
}

/// Load every local image the presentation's Markdown refers to and point the references at the
/// uploads. Reports every problem at once.
pub fn collect(presentation: &mut Briefing, files: &LocalFiles) -> Result<Vec<Image>, ValidationError> {
    let mut value = serde_json::to_value(&*presentation).map_err(|error| ValidationError(error.to_string()))?;
    let mut collector = Collector { files, images: Vec::new(), by_path: HashMap::new(), problems: Vec::new() };
    collector.walk(&mut value);
    if !collector.problems.is_empty() {
        return Err(ValidationError(collector.problems.join("; ")));
    }
    if collector.images.is_empty() {
        return Ok(Vec::new());
    }
    *presentation = serde_json::from_value(value).map_err(|error| ValidationError(error.to_string()))?;
    Ok(collector.images)
}

struct Collector<'a> {
    files: &'a LocalFiles,
    images: Vec<Image>,
    by_path: HashMap<PathBuf, String>,
    problems: Vec<String>,
}

impl Collector<'_> {
    fn walk(&mut self, value: &mut Value) {
        match value {
            Value::String(text) => {
                if let Some(rewritten) = self.rewrite(text) {
                    *text = rewritten;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.walk(item)),
            Value::Object(fields) => fields.values_mut().for_each(|field| self.walk(field)),
            _ => {}
        }
    }

    /// The text with each local image destination replaced, or `None` when it has none.
    fn rewrite(&mut self, text: &str) -> Option<String> {
        if !text.contains("](") {
            return None;
        }
        let mut edits = Vec::new();
        for (event, range) in Parser::new(text).into_offset_iter() {
            let Event::Start(Tag::Image { dest_url, .. }) = event else { continue };
            let Some(path) = local_path(&dest_url) else { continue };
            // The destination as written: after the image's `](`, plain or in `<...>`.
            let source = &text[range.clone()];
            let Some(at) = source.rfind("](").map(|i| range.start + i + 2) else { continue };
            let rest = &text[at..range.end];
            let written = rest.trim_start();
            let start = at + (rest.len() - written.len());
            let len = if let Some(inner) = written.strip_prefix('<') {
                inner.find('>').map(|end| end + 2)
            } else {
                written.find(|c: char| c.is_whitespace() || c == ')')
            };
            let Some(len) = len else { continue };
            if let Some(name) = self.load(&dest_url, &path) {
                edits.push((start..start + len, format!("{SCHEME}{name}")));
            }
        }
        if edits.is_empty() {
            return None;
        }
        let mut out = text.to_owned();
        for (range, replacement) in edits.into_iter().rev() {
            out.replace_range(range, &replacement);
        }
        Some(out)
    }

    fn load(&mut self, written: &str, path: &Path) -> Option<String> {
        let LocalFiles::From(base) = self.files else {
            self.problems.push(format!(
                "image {written}: local files can only be attached through the briefing CLI or a local `briefing mcp` server; use an https:// URL here"
            ));
            return None;
        };
        let path = base.join(path);
        if let Some(name) = self.by_path.get(&path) {
            return Some(name.clone());
        }
        let shown = path.display();
        let bytes = match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_IMAGE_BYTES as u64 => {
                self.problems.push(format!("image {shown} is over {}MB", MAX_IMAGE_BYTES / 1024 / 1024));
                return None;
            }
            Ok(_) => std::fs::read(&path),
            Err(error) => Err(error),
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(error) => {
                self.problems.push(format!("image {shown}: {error}"));
                return None;
            }
        };
        if media_type(&bytes).is_none() {
            self.problems.push(format!("image {shown} is not a PNG, JPEG, GIF or WebP file"));
            return None;
        }
        if self.images.len() == MAX_IMAGES {
            self.problems.push(format!("a briefing can attach at most {MAX_IMAGES} images"));
            return None;
        }
        let total: usize = self.images.iter().map(|image| image.data.len()).sum::<usize>() + bytes.len();
        if total > MAX_TOTAL_IMAGE_BYTES {
            self.problems.push(format!("images over {}MB in total", MAX_TOTAL_IMAGE_BYTES / 1024 / 1024));
            return None;
        }
        let name = (self.images.len() + 1).to_string();
        self.by_path.insert(path, name.clone());
        self.images.push(Image { name: name.clone(), data: bytes });
        Some(name)
    }
}

/// The local file an image destination names, or `None` for a URL the page loads itself.
fn local_path(dest: &str) -> Option<PathBuf> {
    if let Some(rest) = dest.strip_prefix("file://") {
        return url::Url::parse(&format!("file://{rest}")).ok()?.to_file_path().ok();
    }
    if let Some(rest) = dest.strip_prefix("~/") {
        return Some(PathBuf::from(std::env::var_os("HOME")?).join(rest));
    }
    if dest.is_empty() || dest.starts_with('#') || dest.starts_with("//") || has_scheme(dest) {
        return None;
    }
    Some(PathBuf::from(dest))
}

/// `scheme:` at the start, as in `https:` or `data:`.
fn has_scheme(dest: &str) -> bool {
    dest.split_once(':').is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// The hub's check of what a client sent: well-formed, within limits, and each one used.
pub fn validate(presentation: &Briefing, images: &[Image]) -> Result<(), ValidationError> {
    let mut problems = Vec::new();
    if images.len() > MAX_IMAGES {
        problems.push(format!("a briefing can attach at most {MAX_IMAGES} images"));
    }
    if images.iter().map(|image| image.data.len()).sum::<usize>() > MAX_TOTAL_IMAGE_BYTES {
        problems.push(format!("images over {}MB in total", MAX_TOTAL_IMAGE_BYTES / 1024 / 1024));
    }
    let text = serde_json::to_string(presentation).unwrap_or_default();
    let referenced = |name: &str| {
        text.match_indices(SCHEME).any(|(at, _)| {
            let rest = &text[at + SCHEME.len()..];
            rest.strip_prefix(name).is_some_and(|after| !after.starts_with(|c: char| c.is_ascii_digit()))
        })
    };
    let mut seen = std::collections::HashSet::new();
    for image in images {
        if !valid_name(&image.name) || !seen.insert(image.name.as_str()) {
            problems.push(format!("image name {:?} is invalid or repeated", image.name));
            continue;
        }
        if image.data.len() > MAX_IMAGE_BYTES {
            problems.push(format!("image {} is over {}MB", image.name, MAX_IMAGE_BYTES / 1024 / 1024));
        }
        if media_type(&image.data).is_none() {
            problems.push(format!("image {} is not a PNG, JPEG, GIF or WebP file", image.name));
        }
        if !referenced(&image.name) {
            problems.push(format!("image {} is not used by the presentation", image.name));
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(ValidationError(problems.join("; "))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::demo;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    fn with_details(details: &str) -> Briefing {
        let mut briefing = demo();
        briefing.chunks[0].details = Some(details.into());
        briefing
    }

    fn details(briefing: &Briefing) -> &str {
        briefing.chunks[0].details.as_deref().unwrap()
    }

    #[test]
    fn local_images_are_loaded_once_and_their_references_rewritten() {
        let dir = std::env::temp_dir().join(format!("briefing-images-{}", crate::hub::random_id()));
        std::fs::create_dir_all(dir.join("shots")).unwrap();
        std::fs::write(dir.join("shots/a.png"), PNG).unwrap();
        let absolute = dir.join("shots/a.png");
        let mut briefing = with_details(&format!(
            "![one](shots/a.png) and ![again](<{}> \"t\")\n\n![web](https://example.com/x.png) `![code](shots/a.png)`",
            absolute.display()
        ));
        let images = collect(&mut briefing, &LocalFiles::From(dir.clone())).unwrap();
        assert_eq!(images, vec![Image { name: "1".into(), data: PNG.to_vec() }]);
        assert_eq!(
            details(&briefing),
            "![one](briefing-image:1) and ![again](briefing-image:1 \"t\")\n\n![web](https://example.com/x.png) `![code](shots/a.png)`"
        );
        validate(&briefing, &images).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unusable_local_images_are_all_reported() {
        let dir = std::env::temp_dir().join(format!("briefing-images-{}", crate::hub::random_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("page.svg"), "<svg/>").unwrap();
        let mut briefing = with_details("![a](missing.png) ![b](page.svg)");
        let error = collect(&mut briefing, &LocalFiles::From(dir.clone())).unwrap_err().0;
        assert!(error.contains("missing.png") && error.contains("page.svg"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_process_off_the_agents_machine_refuses_local_paths_but_keeps_urls() {
        let mut briefing = with_details("![web](https://example.com/x.png)");
        assert!(collect(&mut briefing, &LocalFiles::Refused).unwrap().is_empty());
        let mut briefing = with_details("![shot](/etc/hostname)");
        assert!(collect(&mut briefing, &LocalFiles::Refused).is_err());
    }

    #[test]
    fn the_hub_rejects_images_that_are_unused_misnamed_or_not_images() {
        let briefing = with_details("![a](briefing-image:1)");
        let image = |name: &str, data: &[u8]| Image { name: name.into(), data: data.to_vec() };
        validate(&briefing, &[image("1", PNG)]).unwrap();
        assert!(validate(&briefing, &[image("1", b"<svg/>")]).is_err());
        assert!(validate(&briefing, &[image("1", PNG), image("2", PNG)]).is_err());
        assert!(validate(&briefing, &[image("../x", PNG)]).is_err());
        let briefing = with_details("![a](briefing-image:12)");
        assert!(validate(&briefing, &[image("1", PNG)]).is_err());
    }

    #[test]
    fn images_travel_as_base64() {
        let image = Image { name: "1".into(), data: PNG.to_vec() };
        let wire = serde_json::to_value(&image).unwrap();
        assert!(wire["data"].is_string());
        assert_eq!(serde_json::from_value::<Image>(wire).unwrap(), image);
    }
}
