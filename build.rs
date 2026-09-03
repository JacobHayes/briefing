//! Embed the browser renderer libraries at build time.
//!
//! The libraries are pinned in `assets/package.json` / `assets/package-lock.json`.
//! If they are not already installed under `assets/node_modules`, this script runs
//! `npm ci` (or `bun install`) to fetch them, then copies the distributable files
//! into `OUT_DIR/vendor` where `src/assets.rs` embeds them with `include_bytes!`.
//!
//! Set `BRIEFING_VENDOR_DIR=/some/dir` to embed pre-built files from a flat
//! directory instead (offline builds); it must contain every file listed below.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const FILES: &[(&str, &str)] = &[
    ("marked.umd.js", "marked/lib/marked.umd.js"),
    ("purify.min.js", "dompurify/dist/purify.min.js"),
    ("highlight.min.js", "@highlightjs/cdn-assets/highlight.min.js"),
    ("mermaid.min.js", "mermaid/dist/mermaid.min.js"),
    ("vega.min.js", "vega/build/vega.min.js"),
    ("vega-lite.min.js", "vega-lite/build/vega-lite.min.js"),
    ("vega-embed.min.js", "vega-embed/build/vega-embed.min.js"),
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let assets_dir = manifest_dir.join("assets");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("vendor");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/package.json");
    println!("cargo:rerun-if-changed=assets/package-lock.json");
    println!("cargo:rerun-if-changed=assets/page.html");
    println!("cargo:rerun-if-changed=assets/demo.json");
    println!("cargo:rerun-if-env-changed=BRIEFING_VENDOR_DIR");

    fs::create_dir_all(&out_dir).expect("create OUT_DIR/vendor");

    if let Some(dir) = env::var_os("BRIEFING_VENDOR_DIR") {
        let dir = PathBuf::from(dir);
        for (name, _) in FILES {
            copy(&dir.join(name), &out_dir.join(name));
        }
        return;
    }

    let node_modules = assets_dir.join("node_modules");
    let missing: Vec<&str> =
        FILES.iter().filter(|(_, rel)| !node_modules.join(rel).is_file()).map(|(name, _)| *name).collect();
    if !missing.is_empty() {
        install(&assets_dir, &missing);
    }
    for (name, rel) in FILES {
        copy(&node_modules.join(rel), &out_dir.join(name));
    }
}

fn copy(from: &Path, to: &Path) {
    println!("cargo:rerun-if-changed={}", from.display());
    if let Err(error) = fs::copy(from, to) {
        panic!("briefing: cannot embed {}: {error}", from.display());
    }
}

fn install(assets_dir: &Path, missing: &[&str]) {
    println!(
        "cargo:warning=briefing: installing browser assets ({}) with npm/bun in {}",
        missing.join(", "),
        assets_dir.display()
    );
    let attempts: [(&str, &[&str]); 2] = [
        ("npm", &["ci", "--ignore-scripts", "--no-audit", "--no-fund", "--loglevel=error"]),
        ("bun", &["install", "--frozen-lockfile", "--ignore-scripts", "--silent"]),
    ];
    let mut failures = Vec::new();
    for (program, args) in attempts {
        match Command::new(program).args(args).current_dir(assets_dir).status() {
            Ok(status) if status.success() => return,
            Ok(status) => failures.push(format!("{program}: exit {status}")),
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }
    panic!(
        "briefing: could not install browser assets into {} ({}). Install Node.js/npm or Bun, \
         or point BRIEFING_VENDOR_DIR at a directory containing: {}",
        assets_dir.display(),
        failures.join("; "),
        FILES.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ")
    );
}
