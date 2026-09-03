//! Static assets embedded into the binary: the briefing page and the renderer libraries
//! (installed from npm by `build.rs`).

pub const PAGE_HTML: &str = include_str!("../assets/page.html");

const JS: &str = "application/javascript; charset=utf-8";

pub struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

macro_rules! vendored {
    ($name:literal) => {
        Asset {
            path: concat!("/briefing-assets/", $name),
            content_type: JS,
            bytes: include_bytes!(concat!(env!("OUT_DIR"), "/vendor/", $name)),
        }
    };
}

pub static ASSETS: &[Asset] = &[
    vendored!("marked.umd.js"),
    vendored!("purify.min.js"),
    vendored!("highlight.min.js"),
    vendored!("mermaid.min.js"),
    vendored!("vega.min.js"),
    vendored!("vega-lite.min.js"),
    vendored!("vega-embed.min.js"),
];

pub fn asset(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

/// The briefing page with the CSP nonce substituted in.
pub fn render_page(nonce: &str) -> String {
    PAGE_HTML.replace("{{NONCE}}", nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_and_assets_are_embedded() {
        assert!(!render_page("abc").contains("{{NONCE}}"));
        assert!(render_page("abc").contains("nonce=\"abc\""));
        for asset in ASSETS {
            assert!(asset.bytes.len() > 10_000, "{} looks empty", asset.path);
            assert!(PAGE_HTML.contains(asset.path), "page does not reference {}", asset.path);
        }
        assert!(asset("/briefing-assets/nope.js").is_none());
    }

    /// The page came from a JS template literal; make sure it was unescaped (a raw copy
    /// ships `\\/` inside regexes and fails to parse) and, when node is available, that
    /// the inline script actually parses.
    #[test]
    fn inline_script_parses() {
        let start =
            PAGE_HTML.find("<script nonce=\"{{NONCE}}\">").expect("script tag") + "<script nonce=\"{{NONCE}}\">".len();
        let end = PAGE_HTML[start..].find("</script>").expect("script end") + start;
        let script = &PAGE_HTML[start..end];
        assert!(!script.contains("\\\\/"), "page script still contains template-literal escapes");
        let dir = std::env::temp_dir().join(format!("briefing-page-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("page.js");
        std::fs::write(&file, script).unwrap();
        match std::process::Command::new("node").arg("--check").arg(&file).output() {
            Ok(output) => assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("node not found; skipping syntax check")
            }
            Err(error) => panic!("{error}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
