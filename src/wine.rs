use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;

const OFFICIAL_DOWNLOAD_URL: &str = "https://api.nrd.nie.163.com/api/v1/release/dl/1?channel=gwqd";
const EULA_URL: &str = "https://uuyc.163.com/contact/20240402/40294_1146065.html";
const SETUP_DLL_OVERRIDES: &str = "wevtapi=b,wtsapi32=b";

struct SetupPrefix(PathBuf);

impl Drop for SetupPrefix {
    fn drop(&mut self) {
        stop_wineserver(&self.0);
    }
}

/// One-time provisioning.  Deliberately quiet: progress on stdout, nothing
/// else.  NetEase binaries are never redistributed — they are fetched from
/// the official feed into a per-user prefix.
pub fn setup(accept_eula: bool, repair: bool) -> Result<()> {
    let mut config = Config::load()?;

    if !config.eula_accepted {
        if !accept_eula {
            println!("{}", t!("setup.eula_prompt", url = EULA_URL));
            anyhow::bail!("{}", t!("setup.eula_required"));
        }
        config.eula_accepted = true;
        config.store()?;
        println!("{}", t!("setup.eula_recorded"));
    }

    let _session_lock = crate::session::acquire_session_lock()?;
    let data = crate::config::data_dir()?;
    let prefix = data.join("wine");
    std::fs::create_dir_all(&prefix).context("creating wine prefix directory")?;

    let cache = data.join("cache");
    std::fs::create_dir_all(&cache).context("creating cache directory")?;
    let _prefix_cleanup = SetupPrefix(prefix.clone());

    if !repair
        && prefix_initialized(&prefix)
        && find_client_dir(&prefix).is_some()
        && client_service_registered(&prefix)
    {
        finish_setup(&prefix)?;
        println!("{}", t!("setup.already_provisioned"));
        return Ok(());
    }

    println!("{}", t!("setup.initializing_prefix"));
    let status = Command::new("wineboot")
        .env("WINEPREFIX", &prefix)
        .env("WINEDEBUG", wine_debug())
        .env("WINEDLLOVERRIDES", SETUP_DLL_OVERRIDES)
        .arg(if repair { "--update" } else { "--init" })
        .status()
        .context("running wineboot")?;
    // `wineboot --init` exits before wineserver flushes system.reg/user.reg
    // to disk (measured ~7s on Wine 11), so a freshly created prefix looks
    // uninitialized to the check below even though wineboot succeeded.  Wait
    // for the server to go idle, which is the point at which the registry
    // files are actually on disk.
    flush_prefix(&prefix);
    if !status.success() || !prefix_initialized(&prefix) {
        anyhow::bail!("{}", t!("setup.prefix_failed", status = status));
    }

    if find_client_dir(&prefix).is_some() && query_client_service(&prefix)? {
        finish_setup(&prefix)?;
        println!("{}", t!("setup.repaired", path = prefix.display()));
        return Ok(());
    }

    println!("{}", t!("setup.downloading", url = OFFICIAL_DOWNLOAD_URL));
    let installer = cache.join("uu-remote-setup.exe");
    let status = Command::new("curl")
        .args([
            "-fL",
            "--retry",
            "3",
            "--progress-bar",
            "-o",
            installer.to_string_lossy().as_ref(),
            OFFICIAL_DOWNLOAD_URL,
        ])
        .status()
        .context("running curl")?;
    if !status.success() {
        anyhow::bail!("{}", t!("setup.download_failed"));
    }

    println!("{}", t!("setup.installing_client"));
    let status = Command::new("wine")
        .env("WINEPREFIX", &prefix)
        .env("WINEDEBUG", wine_debug())
        .env("WINEDLLOVERRIDES", SETUP_DLL_OVERRIDES)
        .arg(installer.to_string_lossy().as_ref())
        .arg("/S")
        .status()
        .context("launching installer")?;
    if !status.success() {
        anyhow::bail!("{}", t!("setup.install_failed", status = status));
    }

    find_client_dir(&prefix).context(t!("setup.client_incomplete").to_string())?;
    if !query_client_service(&prefix)? {
        anyhow::bail!("{}", t!("setup.service_incomplete"));
    }

    finish_setup(&prefix)?;
    println!("{}", t!("setup.done", path = prefix.display().to_string()));
    Ok(())
}

fn finish_setup(prefix: &Path) -> Result<()> {
    // winemenubuilder generated its entries during the install: adopt the
    // start-menu one for the full session, then stop it from producing raw
    // shortcuts ever again, and sweep desktop copies.
    set_dll_override(prefix, "wevtapi", "native")?;
    set_dll_override(prefix, "winemenubuilder", "")?;
    apply_registry_compat(prefix)?;
    let uur_bin = std::env::current_exe().unwrap_or_default();
    let _ = adopt_shortcut(prefix, &uur_bin);
    purge_wine_shortcuts();

    Ok(())
}

pub fn prefix_initialized(prefix: &Path) -> bool {
    ["system.reg", "user.reg"]
        .iter()
        .all(|name| std::fs::metadata(prefix.join(name)).is_ok_and(|metadata| metadata.len() > 0))
}

pub fn webview2_installed(prefix: &Path) -> bool {
    let application = prefix.join("drive_c/Program Files (x86)/Microsoft/EdgeWebView/Application");
    find_dir_recursive(&application, "msedgewebview2.exe", 3)
        .is_some_and(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0))
}

/// The Windows service NetEase's installer registers points at
/// `GameViewerService.exe`.  `GameViewerServer.exe` is a different program —
/// the streaming/terminal server — and never appears in the service
/// ImagePath, so matching on it made every provisioning check fail and left
/// `uur setup` re-downloading and re-installing on an already complete prefix.
const SERVICE_IMAGE: &str = "gameviewerservice.exe";

/// Extract the `ImagePath` recorded for the UU Windows service from the raw
/// `system.reg` text.  Kept pure so it can be tested without a Wine prefix.
fn service_image_path(registry: &str) -> Option<String> {
    let mut service_section = false;
    for line in registry.lines() {
        if let Some(section) = line.strip_prefix('[').and_then(|line| line.split_once(']')) {
            service_section = section
                .0
                .to_ascii_lowercase()
                .ends_with(r"\\services\\gameviewerservice");
        } else if service_section {
            if let Some(value) = line.trim_start().strip_prefix("\"ImagePath\"=") {
                return Some(value.to_ascii_lowercase());
            }
        }
    }
    None
}

/// Read the persisted Wine service registration without starting Wine or
/// showing component installers from `uur doctor`.
pub fn client_service_registered(prefix: &Path) -> bool {
    let Ok(registry) = std::fs::read_to_string(prefix.join("system.reg")) else {
        return false;
    };
    service_image_path(&registry).is_some_and(|image| image.contains(SERVICE_IMAGE))
}

fn query_client_service(prefix: &Path) -> Result<bool> {
    let output = Command::new("wine")
        .env("WINEPREFIX", prefix)
        .env("WINEDEBUG", wine_debug())
        .env("WINEDLLOVERRIDES", SETUP_DLL_OVERRIDES)
        .args([
            "reg",
            "query",
            r"HKLM\System\CurrentControlSet\Services\GameViewerService",
            "/v",
            "ImagePath",
        ])
        .stdin(Stdio::null())
        .output()
        .context("checking the UU Windows service registration")?;
    // Wine may record the service under `ControlSet001` and only alias it as
    // `CurrentControlSet`, and `reg query` output is CRLF; matching the
    // executable name case-insensitively anywhere in stdout covers both.
    Ok(output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .contains(SERVICE_IMAGE))
}

/// Registry compatibility profile, ported from the field-tested reference
/// implementation:
///
/// - msedgewebview2.exe runs under Wine only with a win8 app-compat
///   override — without it the WebView2 process never spawns;
/// - GameViewerServer.exe sees Windows 10 because current UU terminal builds
///   gate their ConPTY transport on the OS version. Native input and display
///   adapters handle the hardware capabilities separately;
/// - the UU client and service prefer their bundled msvcp140 runtime. Wine 10's
///   builtin implementation can stall UU 4.41 before the WebView starts;
/// - the service must auto-start;
/// - X11 focus/XIM tuning and CJK font substitution keep the controller
///   usable.
type RegistryEntry = (String, Vec<(&'static str, &'static str, &'static str)>);

fn registry_compat_entries() -> Vec<RegistryEntry> {
    vec![
        (
            r#"Software\Wine\AppDefaults\msedgewebview2.exe"#.to_string(),
            vec![("Version", "REG_SZ", "win8")],
        ),
        (
            r#"Software\Wine\AppDefaults\GameViewerServer.exe"#.to_string(),
            vec![("Version", "REG_SZ", "win10")],
        ),
        (
            r#"Software\Wine\AppDefaults\gameviewer.exe\DllOverrides"#.to_string(),
            vec![("msvcp140", "REG_SZ", "native,builtin")],
        ),
        (
            r#"Software\Wine\AppDefaults\GameViewerServer.exe\DllOverrides"#.to_string(),
            vec![("msvcp140", "REG_SZ", "native,builtin")],
        ),
        (
            r"System\CurrentControlSet\Services\GameViewerService".to_string(),
            vec![("Start", "REG_DWORD", "2")],
        ),
        (
            r"Software\Wine\X11 Driver".to_string(),
            vec![("UseTakeFocus", "REG_SZ", "N")],
        ),
        (
            r"Software\Wine\AppDefaults\gameviewer.exe\X11 Driver".to_string(),
            vec![("UseXIM", "REG_SZ", "N"), ("UseTakeFocus", "REG_SZ", "Y")],
        ),
        (
            r"Software\Microsoft\Windows NT\CurrentVersion\FontSubstitutes".to_string(),
            vec![
                ("MS Shell Dlg", "REG_SZ", "Noto Sans CJK SC"),
                ("MS Shell Dlg 2", "REG_SZ", "Noto Sans CJK SC"),
            ],
        ),
    ]
}

pub fn apply_registry_compat(prefix: &Path) -> Result<()> {
    for (key, values) in registry_compat_entries() {
        let full = if key.starts_with("System") {
            format!("HKLM\\{key}")
        } else {
            format!("HKCU\\{key}")
        };
        for (value, reg_type, data) in &values {
            let status = Command::new("wine")
                .env("WINEPREFIX", prefix)
                .env("WINEDEBUG", wine_debug())
                .args([
                    "reg", "add", &full, "/v", value, "/t", reg_type, "/d", data, "/f",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .with_context(|| format!("reg add {full} /v {value}"))?;
            if !status.success() {
                anyhow::bail!("Wine rejected registry compatibility setting {full} /v {value}");
            }
        }
    }
    Ok(())
}

pub fn set_dll_override(prefix: &Path, name: &str, mode: &str) -> Result<()> {
    let status = Command::new("wine")
        .env("WINEPREFIX", prefix)
        .env("WINEDEBUG", wine_debug())
        .args([
            "reg",
            "add",
            r"HKCU\Software\Wine\DllOverrides",
            "/v",
            name,
            "/t",
            "REG_SZ",
            "/d",
            mode,
            "/f",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("registering {name} dll override"))?;
    if !status.success() {
        anyhow::bail!("Wine rejected the {name} DLL override");
    }
    Ok(())
}

pub fn set_log_pixels(prefix: &Path, dpi: u32) -> Result<()> {
    let status = log_pixels_command(prefix, dpi)
        .status()
        .context("setting the Wine system DPI")?;
    if !status.success() {
        anyhow::bail!("Wine rejected the system DPI value {dpi}");
    }
    Ok(())
}

fn log_pixels_command(prefix: &Path, dpi: u32) -> Command {
    let mut command = Command::new("wine");
    command
        .env("WINEPREFIX", prefix)
        .env("WINEDEBUG", wine_debug())
        .args([
            "reg",
            "add",
            r"HKCU\Control Panel\Desktop",
            "/v",
            "LogPixels",
            "/t",
            "REG_DWORD",
            "/d",
            &dpi.to_string(),
            "/f",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wine_debug() -> String {
    std::env::var("WINEDEBUG").unwrap_or_else(|_| "-all".to_string())
}

fn stop_wineserver(prefix: &Path) {
    let _ = Command::new("wineserver")
        .env("WINEPREFIX", prefix)
        .arg("-k")
        .status();
}

/// Block until wineserver has flushed the prefix registry to disk.
///
/// `wineboot` returns as soon as it has handed work to wineserver, but
/// `system.reg`/`user.reg` only appear once the server shuts down.  Without
/// this barrier a freshly provisioned prefix is misread as broken.  Both waits
/// are bounded so a stray Wine process cannot hang provisioning.
fn flush_prefix(prefix: &Path) {
    if let Ok(mut child) = Command::new("wineserver")
        .env("WINEPREFIX", prefix)
        .arg("-w")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !prefix_initialized(prefix) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Locate the installed client directory regardless of how NetEase spells
/// the vendor folder ("Netease", "NetEase", ...).  Case matters on Linux.
///
/// NetEase ships the payload both as `<root>/bin/` and as a sibling
/// `<root>/<version>/` directory.  The version directory is *not* the
/// application root: uur reaches the client through `<root>/bin/...` for the
/// terminal proxies and the WebView2 bootstrapper.  The old search returned
/// the first `GameViewerServer.exe` it happened to reach, so when readdir
/// order put `<root>/<version>/` first it returned that instead, every
/// `join("bin")` lookup missed, and `uur run` aborted with a missing WebView2
/// bootstrapper.
///
/// Rank every directory that holds the launcher and pick the best-scoring,
/// shallowest one, so the choice no longer depends on readdir order.
pub fn find_client_dir(prefix: &Path) -> Option<PathBuf> {
    let program_files = prefix.join("drive_c").join("Program Files");
    let entries = std::fs::read_dir(&program_files).ok()?;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case("netease")
        {
            continue;
        }
        let mut roots = Vec::new();
        collect_client_roots(&entry.path(), 4, &mut roots);
        roots.sort_by(|left, right| {
            client_root_rank(right)
                .cmp(&client_root_rank(left))
                .then_with(|| left.components().count().cmp(&right.components().count()))
                .then_with(|| left.cmp(right))
        });
        roots.dedup();
        if let Some(best) = roots.into_iter().next() {
            return Some(best);
        }
    }
    None
}

/// Collect every directory that contains the client launcher.
fn collect_client_roots(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_client_roots(&path, depth - 1, out);
        } else if path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("GameViewer.exe"))
            && non_empty_file(&path)
        {
            if let Some(parent) = path.parent() {
                out.push(parent.to_path_buf());
            }
        }
    }
}

/// Score a candidate root by how well it matches the layout uur drives.
///
/// `bin/GameViewerServer.exe` is the strong signal: the terminal proxies and
/// the WebView2 bootstrapper are all reached through `<root>/bin/`, so a
/// candidate whose `bin` holds the server is the real root, while the
/// `<root>/<version>/` payload copy only holds the launcher at its top level.
fn client_root_rank(candidate: &Path) -> u8 {
    let bin = candidate.join("bin");
    let mut rank = 0;
    if non_empty_file(&bin.join("GameViewerServer.exe")) {
        rank += 2;
    }
    if non_empty_file(&bin.join("MicrosoftEdgeWebview2Setup.exe")) {
        rank += 1;
    }
    if non_empty_file(&candidate.join("GameViewerServer.exe")) {
        rank += 1;
    }
    rank
}

fn non_empty_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

fn find_dir_recursive(start: &Path, needle: &str, depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(start).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(needle))
        {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_dir_recursive(&path, needle, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Preserve an existing Wine-generated launcher as a hidden compatibility
/// alias. Previously pinned desktop icons keep working, but the packaged
/// `uur.desktop` remains the one visible menu entry.
pub fn adopt_shortcut(prefix: &Path, uur_bin: &Path) -> Result<()> {
    use std::fs;

    let mut name = None;
    let users = prefix.join("drive_c/users");
    if let Ok(entries) = fs::read_dir(&users) {
        'outer: for user in entries.flatten() {
            let programs = user
                .path()
                .join("AppData/Roaming/Microsoft/Windows/Start Menu/Programs");
            let Ok(items) = fs::read_dir(&programs) else {
                continue;
            };
            for item in items.flatten() {
                let file_name = item.file_name().to_string_lossy().into_owned();
                if file_name.ends_with(".lnk") && shortcut_targets_gameviewer(&item.path()) {
                    name = Some(file_name.trim_end_matches(".lnk").to_string());
                    break 'outer;
                }
            }
        }
    }
    let Some(name) = name else {
        return Ok(());
    };

    let mut icon_name = String::new();
    let data_home = crate::config::data_dir()?
        .parent()
        .context("no data home")?
        .to_path_buf();
    let icons = data_home.join("icons");
    let mut best = 0u32;
    if let Ok(sizes) = fs::read_dir(icons.join("hicolor")) {
        for size in sizes.flatten() {
            let size_name = size.file_name().to_string_lossy().into_owned();
            let Ok(width) = size_name.split('x').next().unwrap_or("").parse::<u32>() else {
                continue;
            };
            let Ok(apps) = fs::read_dir(size.path().join("apps")) else {
                continue;
            };
            for app in apps.flatten() {
                let file_name = app.file_name().to_string_lossy().into_owned();
                if file_name.contains("GameViewer") && width > best {
                    best = width;
                    icon_name = file_name
                        .trim_end_matches(".png")
                        .trim_end_matches(".0")
                        .to_string();
                }
            }
        }
    }

    let applications = data_home.join("applications");
    std::fs::create_dir_all(&applications)?;
    let entry = applications.join(format!("{name}.desktop"));
    let exec = uur_bin.display();
    let icon_line = if icon_name.is_empty() {
        String::new()
    } else {
        format!("Icon={icon_name}\n")
    };
    std::fs::write(
        &entry,
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={name}\n\
             Comment=UU Remote for Linux (uur)\n\
             Exec={exec} run\n\
             TryExec={exec}\n\
             {icon_line}Terminal=false\n\
             NoDisplay=true\n\
             X-UUR-Managed=true\n\
             X-UUR-Prefix={}\n\
             Categories=Network;RemoteAccess;\n",
            prefix.display()
        ),
    )
    .with_context(|| format!("writing {}", entry.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&entry, fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

fn shortcut_targets_gameviewer(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let searchable: String = bytes
        .into_iter()
        .filter(|byte| *byte != 0)
        .map(|byte| byte as char)
        .collect::<String>()
        .to_ascii_lowercase();
    searchable.contains("gameviewer")
}

/// Remove Wine-generated desktop copies of the client.  The start-menu
/// entry is ours and correct; only Desktop copies are swept.
fn purge_wine_shortcuts() {
    let home = match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home),
        Err(_) => return,
    };
    let matches = |name: &str| {
        let lower = name.to_lowercase();
        lower.contains("uuremote") || lower.contains("gameviewer") || lower.contains("uu remote")
    };
    let dir = home.join("Desktop");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_client_dir;
    use super::log_pixels_command;
    use super::registry_compat_entries;
    use super::service_image_path;
    use std::path::Path;
    use std::path::PathBuf;

    /// Scratch prefix with a real NetEase directory layout, removed on drop.
    struct TestPrefix(PathBuf);

    impl TestPrefix {
        fn create() -> Self {
            let path = std::env::temp_dir().join(format!(
                "uur-client-dir-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Create `<prefix>/drive_c/Program Files/Netease/GameViewer/<rel>`.
        fn write_client_file(&self, rel: &str, contents: &[u8]) {
            let path = self
                .0
                .join("drive_c/Program Files/Netease/GameViewer")
                .join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    impl Drop for TestPrefix {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Populate both payload copies NetEase installs.  The buggy search
    /// returned `<version>/` whenever readdir reached it first, which then
    /// failed every `join("bin")` lookup.
    fn install_both_layouts(prefix: &TestPrefix) {
        for relative in [
            "GameViewer.exe",
            "GameViewerServer.exe",
            "bin/GameViewer.exe",
            "bin/GameViewerServer.exe",
            "4.42.0.2770/GameViewer.exe",
            "4.42.0.2770/GameViewerServer.exe",
        ] {
            prefix.write_client_file(relative, b"payload");
        }
    }

    #[test]
    fn client_root_is_the_directory_holding_bin_not_the_version_directory() {
        let prefix = TestPrefix::create();
        install_both_layouts(&prefix);

        let root = find_client_dir(prefix.path()).expect("client root");

        assert!(
            root.join("bin").join("GameViewerServer.exe").is_file(),
            "the returned root must contain bin/GameViewerServer.exe, got {}",
            root.display()
        );
        assert!(
            !root.file_name().is_some_and(|name| name == "4.42.0.2770"),
            "the version directory must not be selected, got {}",
            root.display()
        );
    }

    #[test]
    fn client_root_is_stable_regardless_of_readdir_order() {
        let prefix = TestPrefix::create();
        install_both_layouts(&prefix);
        let expected = find_client_dir(prefix.path()).unwrap();
        // Repeated lookups must not alternate between the two payload copies.
        for _ in 0..16 {
            assert_eq!(find_client_dir(prefix.path()).as_ref(), Some(&expected));
        }
    }

    #[test]
    fn bin_layout_outranks_a_versioned_payload_copy() {
        let prefix = TestPrefix::create();
        // The versioned copy has the launcher and server at its top level,
        // but no bin/; the real root has the payload in bin/.
        prefix.write_client_file("GameViewer.exe", b"payload");
        prefix.write_client_file("4.42.0.2770/GameViewer.exe", b"payload");
        prefix.write_client_file("4.42.0.2770/GameViewerServer.exe", b"payload");
        prefix.write_client_file("bin/GameViewerServer.exe", b"payload");

        let root = find_client_dir(prefix.path()).expect("client root");

        assert!(
            root.join("bin").join("GameViewerServer.exe").is_file(),
            "the bin layout must win, got {}",
            root.display()
        );
        assert!(
            !root.file_name().is_some_and(|name| name == "4.42.0.2770"),
            "the version directory must not win, got {}",
            root.display()
        );
    }

    #[test]
    fn versioned_copy_is_used_only_when_it_is_the_only_candidate() {
        let prefix = TestPrefix::create();
        prefix.write_client_file("4.42.0.2770/GameViewer.exe", b"payload");

        let root = find_client_dir(prefix.path()).expect("client root");

        assert!(root.ends_with("4.42.0.2770"), "got {}", root.display());
    }

    #[test]
    fn missing_client_reports_none() {
        let prefix = TestPrefix::create();
        std::fs::create_dir_all(prefix.path().join("drive_c/Program Files/Netease")).unwrap();
        assert!(find_client_dir(prefix.path()).is_none());
    }

    /// Real `system.reg` excerpt written by the NetEase installer, escaped
    /// exactly as Wine stores it.  The service ImagePath names
    /// GameViewerService.exe, not GameViewerServer.exe.
    const INSTALLED_SYSTEM_REG: &str = concat!(
        "[System\\\\ControlSet001\\\\Services\\\\GameViewerService] 1791878366\n",
        "#time=1dd5ae8c44b323c\n",
        "\"DependOnService\"=str(7):\"RpcSs\\0Tcpip\\0NDIS\\0AFD\\0\"\n",
        "\"DisplayName\"=\"GameViewerService\"\n",
        "\"ImagePath\"=str(2):\"\\\"C:\\\\Program Files\\\\Netease\\\\GameViewer",
        "\\\\GameViewerService.exe\\\" --service\"\n",
        "\"ObjectName\"=\"LocalSystem\"\n",
        "\"Start\"=dword:00000002\n",
    );

    #[test]
    fn service_registration_matches_the_image_the_installer_registers() {
        // Regression: the check looked for gameviewerserver.exe, which never
        // appears in the service ImagePath, so a complete install was always
        // reported as missing and `uur setup` reinstalled on every run.
        let image = service_image_path(INSTALLED_SYSTEM_REG).expect("ImagePath parsed");
        assert!(image.contains("gameviewerservice.exe"));
        assert!(!image.contains("gameviewerserver.exe"));
    }

    #[test]
    fn missing_service_section_is_not_registered() {
        let registry = "[Software\\\\Wine\\\\DllOverrides] 1\n\"winemenubuilder\"=\"\"\n";
        assert!(service_image_path(registry).is_none());
    }

    #[test]
    fn service_section_without_image_path_is_not_registered() {
        let registry = "[System\\\\ControlSet001\\\\Services\\\\GameViewerService] 1\n\
                        \"Start\"=dword:00000002\n";
        assert!(service_image_path(registry).is_none());
    }

    #[test]
    fn bundled_cpp_runtime_is_preferred_for_both_uu_processes() {
        let entries = registry_compat_entries();
        for executable in ["gameviewer.exe", "GameViewerServer.exe"] {
            let key = format!(r"Software\Wine\AppDefaults\{executable}\DllOverrides");
            let values = entries
                .iter()
                .find_map(|(entry, values)| (entry == &key).then_some(values))
                .unwrap_or_else(|| panic!("missing DLL overrides for {executable}"));
            assert!(values.contains(&("msvcp140", "REG_SZ", "native,builtin")));
        }
    }

    #[test]
    fn system_dpi_command_writes_winecfg_logpixels_value() {
        let command = log_pixels_command(Path::new("/tmp/wine-prefix"), 192);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "reg",
                "add",
                r"HKCU\Control Panel\Desktop",
                "/v",
                "LogPixels",
                "/t",
                "REG_DWORD",
                "/d",
                "192",
                "/f",
            ]
        );
    }
}
