//! Images and videos from the agent's machine, shown in a briefing.
//!
//! An agent puts a local file in any Markdown field as an ordinary image, `![alt](/abs/shot.png)`
//! or `![alt](demo.mp4)`. The process that reads the presentation on the agent's machine (the CLI,
//! or the local `briefing mcp` server) checks each file, rewrites its reference to
//! `briefing-image:<name>` or `briefing-video:<name>`, and streams the files to the hub with the
//! presentation. The hub keeps them beside the briefing's record, for as long as the record, and
//! serves them to the page at `/briefing/<id>/images/<name>`. Remote `https:` URLs and `data:`
//! images are left as they are.
//!
//! The hub's own `/mcp` endpoint runs on the hub, not the agent's machine, so it never reads
//! local paths: that would expose the hub's files to a remote client.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use pulldown_cmark::{Event, Parser, Tag};
use serde_json::Value;

use crate::content::{Briefing, ValidationError};

pub const MAX_ATTACHMENTS: usize = 100;
/// Every attachment on a hub together, so agents cannot fill its disk.
pub const MAX_HUB_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// How many leading bytes [`media_type`] needs.
pub const SNIFF_BYTES: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Video,
}

impl Kind {
    /// What a reference to an attachment of this kind looks like in the stored presentation.
    pub fn scheme(self) -> &'static str {
        match self {
            Kind::Image => "briefing-image:",
            Kind::Video => "briefing-video:",
        }
    }
}

/// The attachment's media type and kind, from its leading bytes. SVG is deliberately absent: it
/// can carry script.
pub fn media_type(head: &[u8]) -> Option<(&'static str, Kind)> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", Kind::Image))
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("image/jpeg", Kind::Image))
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some(("image/gif", Kind::Image))
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        Some(("image/webp", Kind::Image))
    } else if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some(("video/webm", Kind::Video))
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        // MP4 and QuickTime share a container; HEIC and AVIF stills use it too, under brands not
        // listed here. QuickTime is served as MP4, which browsers play when its codecs are H.264
        // or HEVC (a screen recording's), and Chrome does not for `video/quicktime`.
        let brand = &head[8..12];
        let video = [&b"iso"[..], b"mp4", b"avc1", b"M4V", b"qt  ", b"dash"];
        video.iter().any(|prefix| brand.starts_with(prefix)).then_some(("video/mp4", Kind::Video))
    } else {
        None
    }
}

/// A name the hub accepts for an attachment, which is also safe as a file name.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 3 && name.bytes().all(|b| b.is_ascii_digit())
}

/// What the kinds of file a briefing can show are, for messages.
pub const KINDS: &str = "a PNG, JPEG, GIF or WebP image, or an MP4, MOV or WebM video";

fn gigabytes(bytes: u64) -> u64 {
    bytes / 1024 / 1024 / 1024
}

/// Where local image paths come from.
#[derive(Debug, Clone)]
pub enum LocalFiles {
    /// Read them, resolving relative paths against this directory.
    From(PathBuf),
    /// Refuse them: this process does not run on the agent's machine.
    Refused,
}

/// A local file to send with a presentation: read when it is uploaded, not before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAttachment {
    pub name: String,
    pub path: PathBuf,
    pub len: u64,
}

/// Check every local file the presentation's Markdown refers to and point the references at the
/// uploads. Reports every problem at once.
pub fn collect(presentation: &mut Briefing, files: &LocalFiles) -> Result<Vec<LocalAttachment>, ValidationError> {
    let mut value = serde_json::to_value(&*presentation).map_err(|error| ValidationError(error.to_string()))?;
    let mut collector = Collector { files, attachments: Vec::new(), by_path: HashMap::new(), problems: Vec::new() };
    collector.walk(&mut value);
    let total: u64 = collector.attachments.iter().map(|a| a.len).sum();
    if total > MAX_HUB_BYTES {
        collector.problems.push(format!("attachments over {}GB in total", gigabytes(MAX_HUB_BYTES)));
    }
    if !collector.problems.is_empty() {
        return Err(ValidationError(collector.problems.join("; ")));
    }
    if collector.attachments.is_empty() {
        return Ok(Vec::new());
    }
    *presentation = serde_json::from_value(value).map_err(|error| ValidationError(error.to_string()))?;
    Ok(collector.attachments)
}

struct Collector<'a> {
    files: &'a LocalFiles,
    attachments: Vec<LocalAttachment>,
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
            if let Some(reference) = self.load(&dest_url, &path) {
                edits.push((start..start + len, reference));
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

    /// The reference to use for the file at `path`, once it is checked.
    fn load(&mut self, written: &str, path: &Path) -> Option<String> {
        let LocalFiles::From(base) = self.files else {
            self.problems.push(format!(
                "attachment {written}: local files can only be attached through the briefing CLI or a local `briefing mcp` server; use an https:// URL here"
            ));
            return None;
        };
        let path = base.join(path);
        if let Some(reference) = self.by_path.get(&path) {
            return Some(reference.clone());
        }
        let shown = path.display();
        let (len, kind) = match sniff(&path) {
            Ok(Some(found)) => found,
            Ok(None) => {
                self.problems.push(format!("attachment {shown} is not {KINDS}"));
                return None;
            }
            Err(error) => {
                self.problems.push(format!("attachment {shown}: {error}"));
                return None;
            }
        };
        if self.attachments.len() == MAX_ATTACHMENTS {
            self.problems.push(format!("a briefing can attach at most {MAX_ATTACHMENTS} files"));
            return None;
        }
        let name = (self.attachments.len() + 1).to_string();
        let reference = format!("{}{name}", kind.scheme());
        self.by_path.insert(path.clone(), reference.clone());
        self.attachments.push(LocalAttachment { name, path, len });
        Some(reference)
    }
}

/// The size and kind of the regular file at `path`, or `None` when it is not one briefing shows.
/// Devices and pipes are refused before reading: they may never end.
fn sniff(path: &Path) -> std::io::Result<Option<(u64, Kind)>> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    std::fs::File::open(path)?.take(SNIFF_BYTES as u64).read_to_end(&mut head)?;
    Ok(media_type(&head).map(|(_, kind)| (meta.len(), kind)))
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

/// The hub's check of the attachments a client sent, by name and kind: within the count, and
/// each one used as what it is.
pub fn validate(presentation: &Briefing, attachments: &[(String, Kind)]) -> Result<(), ValidationError> {
    let mut problems = Vec::new();
    if attachments.len() > MAX_ATTACHMENTS {
        problems.push(format!("a briefing can attach at most {MAX_ATTACHMENTS} files"));
    }
    let text = serde_json::to_string(presentation).unwrap_or_default();
    let referenced = |reference: &str| {
        text.match_indices(reference)
            .any(|(at, _)| !text[at + reference.len()..].starts_with(|c: char| c.is_ascii_digit()))
    };
    for (name, kind) in attachments {
        if !referenced(&format!("{}{name}", kind.scheme())) {
            problems.push(format!("attachment {name} is not used by the presentation"));
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(ValidationError(problems.join("; "))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::demo;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";
    const MP4: &[u8] = b"\0\0\0\x20ftypisomrest";

    fn with_details(details: &str) -> Briefing {
        let mut briefing = demo();
        briefing.chunks[0].details = Some(details.into());
        briefing
    }

    fn details(briefing: &Briefing) -> &str {
        briefing.chunks[0].details.as_deref().unwrap()
    }

    #[test]
    fn local_files_are_checked_once_and_their_references_rewritten_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("shots")).unwrap();
        std::fs::write(dir.path().join("shots/a.png"), PNG).unwrap();
        std::fs::write(dir.path().join("demo.mov"), MP4).unwrap();
        let absolute = dir.path().join("shots/a.png");
        let mut briefing = with_details(&format!(
            "![one](shots/a.png) and ![again](<{}> \"t\") ![clip](demo.mov)\n\n![web](https://example.com/x.png) `![code](shots/a.png)`",
            absolute.display()
        ));
        let attachments = collect(&mut briefing, &LocalFiles::From(dir.path().to_path_buf())).unwrap();
        let summary: Vec<_> = attachments.iter().map(|a| (a.name.as_str(), a.len)).collect();
        assert_eq!(summary, [("1", PNG.len() as u64), ("2", MP4.len() as u64)]);
        assert_eq!(
            details(&briefing),
            "![one](briefing-image:1) and ![again](briefing-image:1 \"t\") ![clip](briefing-video:2)\n\n![web](https://example.com/x.png) `![code](shots/a.png)`"
        );
        validate(&briefing, &[("1".into(), Kind::Image), ("2".into(), Kind::Video)]).unwrap();
    }

    #[test]
    fn unusable_local_files_are_all_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("page.svg"), "<svg/>").unwrap();
        std::fs::create_dir(dir.path().join("folder")).unwrap();
        let mut briefing = with_details("![a](missing.png) ![b](page.svg) ![c](folder) ![d](/dev/zero)");
        let error = collect(&mut briefing, &LocalFiles::From(dir.path().to_path_buf())).unwrap_err().0;
        for name in ["missing.png", "page.svg", "folder", "/dev/zero"] {
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn a_process_off_the_agents_machine_refuses_local_paths_but_keeps_urls() {
        let mut briefing = with_details("![web](https://example.com/x.png)");
        assert!(collect(&mut briefing, &LocalFiles::Refused).unwrap().is_empty());
        let mut briefing = with_details("![shot](/etc/hostname)");
        assert!(collect(&mut briefing, &LocalFiles::Refused).is_err());
    }

    #[test]
    fn the_hub_rejects_attachments_that_are_unused_or_used_as_another_kind() {
        let briefing = with_details("![a](briefing-image:1)");
        validate(&briefing, &[("1".into(), Kind::Image)]).unwrap();
        assert!(validate(&briefing, &[("1".into(), Kind::Video)]).is_err());
        assert!(validate(&briefing, &[("1".into(), Kind::Image), ("2".into(), Kind::Image)]).is_err());
        let briefing = with_details("![a](briefing-image:12)");
        assert!(validate(&briefing, &[("1".into(), Kind::Image)]).is_err());
    }

    #[test]
    fn videos_are_told_from_stills_in_the_same_container() {
        assert_eq!(media_type(MP4), Some(("video/mp4", Kind::Video)));
        assert_eq!(media_type(b"\0\0\0\x14ftypqt  \0\0"), Some(("video/mp4", Kind::Video)));
        assert_eq!(media_type(&[0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0]), Some(("video/webm", Kind::Video)));
        assert_eq!(media_type(b"\0\0\0\x18ftypheic\0\0"), None);
        assert_eq!(media_type(b"<svg/>"), None);
    }
}
