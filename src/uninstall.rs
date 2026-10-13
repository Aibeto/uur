//! Explicit per-user removal. Distribution-owned files stay with the package
//! manager; a source installation is identified by its installer receipt.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy)]
enum Removal {
    File,
    Tree,
    EmptyDirectory,
}

struct Plan {
    home: PathBuf,
    data: PathBuf,
    config: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    entries: BTreeMap<PathBuf, Removal>,
    source_install: bool,
}

pub fn run(purge: bool, dry_run: bool, yes: bool) -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        anyhow::bail!("{}", t!("uninstall.no_root"));
    }
    let mut plan = Plan::build(purge)?;
    println!("{}", t!("uninstall.plan"));
    for (path, removal) in &plan.entries {
        if !matches!(removal, Removal::EmptyDirectory) {
            println!("  {}", path.display());
        }
    }
    if purge {
        println!("{}", t!("uninstall.purge_warning"));
    } else {
        println!("{}", t!("uninstall.keep_data"));
    }
    println!("{}", t!("uninstall.package_hint"));
    if dry_run {
        println!("{}", t!("uninstall.dry_run"));
        return Ok(());
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("{}", t!("uninstall.confirm_required"));
        }
        print!("{} ", t!("uninstall.confirm"));
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("{}", t!("uninstall.cancelled"));
            return Ok(());
        }
    }

    crate::session::stop()?;
    // Keep setup/run excluded until the directory cleanup has finished.
    let _session_lock = crate::session::acquire_session_lock()?;
    stop_prefix(&plan.data.join("wine"))?;
    // Revalidate the displayed targets after stopping the session. Do not add
    // newly discovered files to a plan the user has already confirmed.
    plan.validate()?;
    // Remove integrations first, then the trees; the running ELF can be
    // unlinked on Linux, so source installs need no second helper process.
    let trees: Vec<_> = plan
        .entries
        .iter()
        .filter_map(|(path, removal)| matches!(removal, Removal::Tree).then_some(path.clone()))
        .collect();
    plan.entries.retain(|path, removal| {
        !matches!(removal, Removal::File)
            || !trees
                .iter()
                .any(|root| path != root && path.starts_with(root))
    });
    let mut removals: Vec<_> = plan
        .entries
        .iter()
        .filter(|(_, removal)| !matches!(removal, Removal::EmptyDirectory))
        .collect();
    // Keep the receipt until all installed files are removed, so a failed
    // cleanup can be retried from another copy of uur.
    removals.sort_by_key(
        |(path, _)| match path.file_name().and_then(|name| name.to_str()) {
            Some("user-install") => 2,
            Some("uur") if path.parent().and_then(Path::file_name) == Some("bin".as_ref()) => 1,
            _ => 0,
        },
    );
    for (path, removal) in removals {
        remove(path, *removal)?;
    }
    for (path, removal) in plan.entries.iter().rev() {
        if matches!(removal, Removal::EmptyDirectory) {
            remove(path, *removal)?;
        }
    }
    println!("{}", t!("uninstall.done"));
    if !plan.source_install {
        println!("{}", t!("uninstall.package_hint"));
    }
    Ok(())
}

impl Plan {
    fn build(purge: bool) -> Result<Self> {
        let home = absolute(PathBuf::from(
            std::env::var_os("HOME").context("HOME is unset")?,
        ))?;
        let data_home = xdg_home("XDG_DATA_HOME", &home.join(".local/share"))?;
        let config_home = xdg_home("XDG_CONFIG_HOME", &home.join(".config"))?;
        let state_home = xdg_home("XDG_STATE_HOME", &home.join(".local/state"))?;
        let runtime_home = xdg_home(
            "XDG_RUNTIME_DIR",
            &PathBuf::from(format!("/tmp/uur-{}", unsafe { libc::geteuid() })),
        )?;
        let mut plan = Self {
            home: home.clone(),
            data: data_home.join("uur"),
            config: config_home.join("uur"),
            state: state_home.join("uur"),
            runtime: runtime_home.join("uur"),
            entries: BTreeMap::new(),
            source_install: false,
        };
        plan.entries.insert(plan.runtime.clone(), Removal::Tree);
        if purge {
            for path in [&plan.data, &plan.config, &plan.state] {
                plan.entries.insert(path.clone(), Removal::Tree);
            }
        }
        let default_scope = data_home == home.join(".local/share");
        let mut desktops = BTreeSet::new();
        scan_files(&data_home.join("applications"), 8, &mut desktops)?;
        // Old releases placed compatibility aliases in ~/.local/share even
        // when XDG_DATA_HOME was set. Only entries tied to this prefix qualify.
        scan_files(&home.join(".local/share/applications"), 8, &mut desktops)?;
        scan_files(&desktop_dir(&home, &config_home), 1, &mut desktops)?;
        let autostart = config_home.join("autostart/uur.desktop");
        if autostart.exists() {
            desktops.insert(autostart);
        }
        let mut removed_icons = BTreeSet::new();
        let mut kept_icons = BTreeSet::new();
        let mut remove_default_icons = default_scope;
        for path in desktops {
            if path
                .extension()
                .is_none_or(|extension| extension != "desktop")
            {
                continue;
            }
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let icon = desktop_value(&content, "Icon").unwrap_or_default();
            if plan.owns_desktop(&content, default_scope) {
                remove_default_icons |= path.starts_with(home.join(".local/share/applications"))
                    || path.starts_with(desktop_dir(&home, &config_home));
                plan.entries.insert(path, Removal::File);
                removed_icons.insert(icon.to_string());
            } else {
                kept_icons.insert(icon.to_string());
            }
        }
        let mut icon_roots = BTreeSet::from([data_home.join("icons")]);
        if remove_default_icons {
            icon_roots.insert(home.join(".local/share/icons"));
        }
        for icons in icon_roots {
            let mut files = BTreeSet::new();
            scan_files(&icons, 6, &mut files)?;
            for path in files {
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                let alias = stem.strip_suffix(".0").unwrap_or(stem);
                if (removed_icons.contains(stem) || removed_icons.contains(alias))
                    && !kept_icons.contains(stem)
                    && !kept_icons.contains(alias)
                {
                    plan.entries.insert(path, Removal::File);
                }
            }
        }
        plan.source_files()?;
        plan.validate()?;
        Ok(plan)
    }

    fn owns_desktop(&self, content: &str, default_scope: bool) -> bool {
        let prefix = self.data.join("wine");
        if let Some(recorded) = desktop_value(content, "X-UUR-Prefix") {
            return desktop_value(content, "X-UUR-Managed") == Some("true")
                && recorded == prefix.to_string_lossy();
        }
        let managed = desktop_value(content, "X-UUR-Managed") == Some("true")
            || desktop_value(content, "Comment") == Some("Start the managed UU Remote session")
            || desktop_value(content, "Comment") == Some("UU Remote for Linux (uur)");
        if managed && default_scope {
            return true;
        }
        // Wine writes an explicit WINEPREFIX assignment in its launchers.
        // Match the entire quoted/unquoted value, not a filename substring.
        let exec = desktop_value(content, "Exec").unwrap_or_default();
        let prefix = prefix.to_string_lossy();
        let references_prefix = [
            format!("WINEPREFIX=\"{prefix}\" "),
            format!("WINEPREFIX='{prefix}' "),
            format!("WINEPREFIX={prefix} "),
        ]
        .iter()
        .any(|assignment| exec.contains(assignment));
        references_prefix && exec.to_ascii_lowercase().contains("gameviewer")
    }

    fn source_files(&mut self) -> Result<()> {
        let executable = std::env::current_exe()?;
        let mut prefixes = BTreeSet::from([self.home.join(".local")]);
        if let Some(prefix) = executable.parent().and_then(Path::parent) {
            if executable.parent().and_then(Path::file_name) == Some("bin".as_ref()) {
                prefixes.insert(prefix.to_path_buf());
            }
        }
        if let Some(prefix) = std::env::var_os("UUR_INSTALL_PREFIX") {
            prefixes.insert(absolute(PathBuf::from(prefix))?);
        }
        for prefix in prefixes {
            let receipt = prefix.join("lib/uur/user-install");
            let marked = std::fs::read_to_string(&receipt).is_ok_and(|content| {
                content == format!("uur-user-install-v1\n{}\n", self.data.display())
            });
            // Support the previous source installer, but only while executing
            // its ~/.local/bin/uur and with the matching generated menu entry.
            let legacy = prefix == self.home.join(".local")
                && std::fs::canonicalize(prefix.join("bin/uur")).ok() == Some(executable.clone())
                && std::fs::read_to_string(prefix.join("share/applications/uur.desktop"))
                    .is_ok_and(|content| {
                        desktop_value(&content, "TryExec")
                            == Some(prefix.join("bin/uur").to_string_lossy().as_ref())
                    });
            if !marked && !legacy {
                continue;
            }
            self.source_install = true;
            for relative in [
                "bin/uur",
                "lib/uur/user-install",
                "lib/uur/uur-pw-capture",
                "lib/uur/uur-wl-capture",
                "lib/uur/hook/wevtapi.dll",
                "lib/uur/hook/uur-hook.dll",
                "lib/uur/hook/wtsapi32.dll",
                "lib/uur/hook/winlogon.exe",
                "lib/uur/hook/uur-terminal-proxy.exe",
                "lib/uur/hook/uur-mux-proxy.exe",
                "lib/uur/hook/uur-launch-proxy.exe",
                "share/applications/uur.desktop",
                "share/icons/hicolor/256x256/apps/uur.png",
            ] {
                let path = prefix.join(relative);
                if std::fs::symlink_metadata(&path).is_ok() {
                    self.entries.insert(path, Removal::File);
                }
            }
            for relative in ["lib/uur/hook", "lib/uur"] {
                self.entries
                    .insert(prefix.join(relative), Removal::EmptyDirectory);
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        // Session shutdown/locking access these paths before tree removal.
        // Reject redirected state rather than following a link while writing
        // the lock or stopping an unrelated Wine prefix.
        for path in [
            &self.data,
            &self.state,
            &self.runtime,
            &self.state.join("session.lock"),
            &self.data.join("wine"),
        ] {
            if path.is_symlink() {
                anyhow::bail!("refusing redirected session state: {}", path.display());
            }
            if std::fs::metadata(path)
                .is_ok_and(|metadata| metadata.uid() != unsafe { libc::geteuid() })
            {
                anyhow::bail!("session state belongs to another user: {}", path.display());
            }
        }
        for (path, removal) in &self.entries {
            let path = absolute(path.clone())?;
            // Resolve ancestor links, but never resolve the final link: remove
            // symlink entries themselves, not the objects they point at.
            let resolved = path
                .parent()
                .and_then(|parent| std::fs::canonicalize(parent).ok())
                .map(|parent| parent.join(path.file_name().unwrap_or_default()))
                .unwrap_or_else(|| path.clone());
            if path == self.home || resolved == self.home || path.parent().is_none() {
                anyhow::bail!("refusing to remove {}", path.display());
            }
            // Never allow a receipt or an XDG override to select system trees.
            for system in [
                "/usr", "/etc", "/opt", "/var", "/nix", "/bin", "/lib", "/sbin", "/boot", "/dev",
                "/proc", "/sys",
            ] {
                if path.starts_with(system) || resolved.starts_with(system) {
                    anyhow::bail!("refusing to remove system path {}", path.display());
                }
            }
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if metadata.uid() != unsafe { libc::geteuid() } {
                        anyhow::bail!(
                            "refusing to remove a path owned by another user: {}",
                            path.display()
                        );
                    }
                    if matches!(removal, Removal::Tree)
                        && !metadata.file_type().is_symlink()
                        && (path.join(".git").exists() || path.join("Cargo.toml").exists())
                    {
                        anyhow::bail!("refusing to remove a source checkout: {}", path.display());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("checking {}", path.display()))
                }
            }
        }
        Ok(())
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!(
            "uninstall requires absolute XDG/install paths: {}",
            path.display()
        );
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                normalized.pop();
            }
            _ => {}
        }
    }
    Ok(normalized)
}

fn xdg_home(variable: &str, fallback: &Path) -> Result<PathBuf> {
    absolute(
        std::env::var_os(variable)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| fallback.to_path_buf()),
    )
}

fn desktop_value<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    let mut main_section = false;
    for line in content.lines() {
        if line.starts_with('[') {
            main_section = line == "[Desktop Entry]";
        } else if main_section {
            if let Some((name, value)) = line.split_once('=') {
                if name == key {
                    return Some(value);
                }
            }
        }
    }
    None
}

fn desktop_dir(home: &Path, config_home: &Path) -> PathBuf {
    // user-dirs.dirs is data, not a shell script: never source it.
    std::fs::read_to_string(config_home.join("user-dirs.dirs"))
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                let value = line.strip_prefix("XDG_DESKTOP_DIR=\"")?.strip_suffix('"')?;
                let path = value
                    .strip_prefix("$HOME/")
                    .map(|suffix| home.join(suffix))
                    .unwrap_or_else(|| PathBuf::from(value));
                (path.is_absolute() && path != home).then_some(path)
            })
        })
        .unwrap_or_else(|| home.join("Desktop"))
}

fn scan_files(dir: &Path, depth: u8, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    if depth == 0 {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("listing {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            scan_files(&entry.path(), depth - 1, files)?;
        } else if kind.is_file() {
            files.insert(entry.path());
        }
    }
    Ok(())
}

fn stop_prefix(prefix: &Path) -> Result<()> {
    if !prefix.is_dir() || prefix.is_symlink() {
        return Ok(());
    }
    match Command::new("wineserver")
        .env("WINEPREFIX", prefix)
        .arg("-k")
        .stdin(Stdio::null())
        .status()
    {
        // -k exits 1 when stop already shut down this prefix's server. The
        // following -w, with a deadline, is the actual shutdown check.
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("stopping the managed wineserver"),
    }
    let mut wait = Command::new("wineserver")
        .env("WINEPREFIX", prefix)
        .arg("-w")
        .stdin(Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = wait.try_wait()? {
            if !status.success() {
                anyhow::bail!("wineserver shutdown failed");
            }
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let _ = wait.kill();
            let _ = wait.wait();
            anyhow::bail!("managed wineserver is still running; no files were removed");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn remove(path: &Path, removal: Removal) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("checking {}", path.display())),
    };
    let result = match removal {
        Removal::Tree if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)
        }
        Removal::EmptyDirectory if metadata.is_dir() => std::fs::remove_dir(path),
        _ => std::fs::remove_file(path),
    };
    match result {
        Err(error)
            if matches!(removal, Removal::EmptyDirectory)
                && error.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
        {
            Ok(())
        }
        other => other.with_context(|| format!("removing {}", path.display())),
    }
}
