//! Optional XDG login autostart, independent of desktop environment.

use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn enable() -> Result<()> {
    let path = desktop_path()?;
    let parent = path.parent().context("autostart path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let executable = std::env::current_exe().context("resolving uur executable")?;
    let exec = desktop_exec_argument(&executable.to_string_lossy());
    let prefix = crate::config::data_dir()?.join("wine");
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=UU Remote\nComment=Start the managed UU Remote session\nExec={exec} run\nIcon=uur\nTerminal=false\nX-GNOME-Autostart-enabled=true\nX-UUR-Managed=true\nX-UUR-Prefix={}\n", prefix.display()
        ),
    )?;
    println!(
        "{}",
        t!("autostart.enabled", path = path.display().to_string())
    );
    Ok(())
}

pub fn disable() -> Result<()> {
    let path = desktop_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    println!("{}", t!("autostart.disabled"));
    Ok(())
}

pub fn status() -> Result<()> {
    let path = desktop_path()?;
    println!(
        "{}",
        if path.is_file() {
            t!(
                "autostart.status_enabled",
                path = path.display().to_string()
            )
        } else {
            t!("autostart.status_disabled")
        }
    );
    Ok(())
}

fn desktop_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("cannot determine XDG configuration directory")?;
    Ok(base.join("autostart/uur.desktop"))
}

fn desktop_exec_argument(value: &str) -> String {
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'.' | b':')
    }) {
        return value.to_string();
    }
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::desktop_exec_argument;

    #[test]
    fn desktop_exec_paths_are_quoted_only_when_needed() {
        assert_eq!(desktop_exec_argument("/usr/bin/uur"), "/usr/bin/uur");
        assert_eq!(
            desktop_exec_argument("/opt/UU Remote/uur"),
            "\"/opt/UU Remote/uur\""
        );
    }
}
