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
pub fn find_client_dir(prefix: &Path) -> Option<PathBuf> {
    let program_files = prefix.join("drive_c").join("Program Files");
    let entries = std::fs::read_dir(&program_files).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().eq_ignore_ascii_case("netease") {
            let vendor = entry.path();
            if let Some(found) = find_dir_recursive(&vendor, "GameViewerServer.exe", 4) {
                let bin = found.parent()?;
                let candidate = if bin
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
                {
                    bin.parent()?.to_path_buf()
                } else {
                    bin.to_path_buf()
                };
                if std::fs::metadata(candidate.join("GameViewer.exe"))
                    .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
                    && std::fs::metadata(&found).is_ok_and(|metadata| metadata.len() > 0)
                {
                    return Some(candidate);
                }
            }
        }
    }
    None
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
    use super::log_pixels_command;
    use super::registry_compat_entries;
    use super::service_image_path;
    use std::path::Path;

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
