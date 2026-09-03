//! Opening the briefing URL in the system browser.

use std::time::Duration;

/// Linux needs a graphical display for `xdg-open` to be useful; macOS and Windows use
/// native openers that do not depend on these variables.
pub fn should_open_system_browser(platform: &str, display: Option<&str>, wayland: Option<&str>) -> bool {
    if platform != "linux" {
        return true;
    }
    display.is_some_and(|v| !v.trim().is_empty()) || wayland.is_some_and(|v| !v.trim().is_empty())
}

pub fn graphical_session_available() -> bool {
    should_open_system_browser(
        std::env::consts::OS,
        std::env::var("DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
    )
}

/// Try to open `url`. `Ok(false)` means there was no graphical session to use;
/// `Err` means an opener was attempted and failed.
pub async fn open_url(url: &str) -> anyhow::Result<bool> {
    if let Ok(custom) = std::env::var("BRIEFING_BROWSER")
        && !custom.trim().is_empty()
    {
        return run(custom.trim(), &[url]).await.map(|_| true);
    }
    if !graphical_session_available() {
        return Ok(false);
    }
    match std::env::consts::OS {
        "macos" => run("open", &[url]).await?,
        "windows" => run("cmd", &["/c", "start", "", url]).await?,
        _ => run("xdg-open", &[url]).await?,
    }
    Ok(true)
}

async fn run(command: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(command).args(args).stdin(std::process::Stdio::null()).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{command} timed out"))?
    .map_err(|e| anyhow::anyhow!("failed to run {command}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit {}", output.status)
        };
        anyhow::bail!("Failed to open the briefing in the browser: {detail}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_needs_a_display() {
        assert!(!should_open_system_browser("linux", None, None));
        assert!(!should_open_system_browser("linux", Some(" "), None));
        assert!(should_open_system_browser("linux", Some(":0"), None));
        assert!(should_open_system_browser("linux", None, Some("wayland-0")));
        assert!(should_open_system_browser("macos", None, None));
        assert!(should_open_system_browser("windows", None, None));
    }
}
