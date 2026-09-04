//! Static assets embedded into the binary: the briefing page, the hub dashboard, and the
//! renderer libraries (installed from npm by `build.rs`).

pub const PAGE_HTML: &str = include_str!("../assets/page.html");
pub const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");

/// URL prefix the page loads the vendored libraries from.
pub const ASSET_PREFIX: &str = "/briefing-assets/";

macro_rules! vendored {
    ($name:literal) => {
        ($name, include_bytes!(concat!(env!("OUT_DIR"), "/vendor/", $name)) as &[u8])
    };
}

/// `(file name, bytes)`; every entry is JavaScript.
pub static ASSETS: &[(&str, &[u8])] = &[
    vendored!("marked.umd.js"),
    vendored!("purify.min.js"),
    vendored!("highlight.min.js"),
    vendored!("mermaid.min.js"),
    vendored!("vega.min.js"),
    vendored!("vega-lite.min.js"),
    vendored!("vega-embed.min.js"),
];

pub fn asset(name: &str) -> Option<&'static [u8]> {
    ASSETS.iter().find(|(n, _)| *n == name).map(|(_, bytes)| *bytes)
}

/// An embedded page with the CSP nonce substituted in.
pub fn render(html: &str, nonce: &str) -> String {
    html.replace("{{NONCE}}", nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_and_assets_are_embedded() {
        assert!(!render(PAGE_HTML, "abc").contains("{{NONCE}}"));
        assert!(render(PAGE_HTML, "abc").contains("nonce=\"abc\""));
        for (name, bytes) in ASSETS {
            assert!(bytes.len() > 10_000, "{name} looks empty");
            assert!(PAGE_HTML.contains(&format!("{ASSET_PREFIX}{name}")), "page does not reference {name}");
        }
        assert!(asset("nope.js").is_none());
    }

    /// The page came from a JS template literal; make sure it was unescaped (a raw copy
    /// ships `\\/` inside regexes and fails to parse) and, when node is available, that
    /// the inline script actually parses.
    #[test]
    fn inline_script_parses() {
        for (name, html) in [("page", PAGE_HTML), ("dashboard", DASHBOARD_HTML)] {
            check_inline_script(name, html);
        }
    }

    fn check_inline_script(name: &str, html: &str) {
        let start =
            html.find("<script nonce=\"{{NONCE}}\">").expect("script tag") + "<script nonce=\"{{NONCE}}\">".len();
        let end = html[start..].find("</script>").expect("script end") + start;
        let script = &html[start..end];
        assert!(!script.contains("\\\\/"), "{name} script still contains template-literal escapes");
        let dir = std::env::temp_dir().join(format!("briefing-page-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{name}.js"));
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
