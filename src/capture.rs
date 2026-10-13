//! Portal side of the capture path.
//!
//! Negotiates an xdg-desktop-portal ScreenCast session, obtains a private
//! PipeWire file descriptor and the video node id, then hands both to the
//! C capture helper (`uur-pw-capture`) which runs the PipeWire stream and
//! publishes frames into a private XDG runtime file (see
//! docs/capture-protocol.md).
//!
//! Consent: the first run shows the compositor's share dialog once.  The
//! returned restore token is stored in uur's config; every later run hands
//! it back with PersistMode::Persistent and the portal restores the choice
//! silently — no dialog, no local interaction, which is the whole point on
//! a headless remote host.

use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::Config;

/// Run the capture path until interrupted.
pub fn run_standalone() -> Result<()> {
    // Public capture uses the same lifecycle lock as run/setup so stop and
    // uninstall can also shut down a diagnostic ScreenCast session. Internal
    // capture already runs under the supervisor's lock.
    let _session_lock = crate::session::acquire_session_lock()?;
    run()
}

pub fn run() -> Result<()> {
    let mut config = Config::load()?;
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())?;
    if config.capture_backend == "wayland" {
        if config.capture_source != "monitor" {
            anyhow::bail!("direct Wayland capture requires a monitor source");
        }
        let mut command = Command::new(find_helper("uur-wl-capture")?);
        command.env("UUR_FRAME_PATH", frame_path()?);
        if let Some(output) = &config.capture_output {
            command.env("UUR_CAPTURE_OUTPUT", output);
        }
        let mut helper = CaptureHelper(command.spawn().context("starting direct Wayland capture")?);
        println!("{}", t!("capture.direct_started"));
        while !stopping.load(Ordering::Relaxed) {
            if let Some(status) = helper.0.try_wait()? {
                if !status.success() {
                    anyhow::bail!("direct Wayland capture failed ({status})");
                }
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        return Ok(());
    }
    if config.capture_backend != "portal" {
        anyhow::bail!("unknown capture backend {:?}", config.capture_backend);
    }
    let runtime = tokio::runtime::Runtime::new().context("creating capture runtime")?;
    // The portal's D-Bus connection runs on this runtime. Keep it running
    // throughout capture, including on capture-only compositors such as niri.
    runtime.block_on(async {
        let (pw_fd, node) = tokio::select! {
            result = portal_session(&mut config) => result?,
            _ = wait_for_stop(&stopping) => return Ok(()),
        };
        println!("{}", t!("capture.session_ready", node = node));
        let mut helper = CaptureHelper(spawn_helper(pw_fd, node)?);
        loop {
            if stopping.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(status) = helper.0.try_wait().context("waiting for capture helper")? {
                if !status.success() {
                    anyhow::bail!("{}", t!("capture.helper_failed"));
                }
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
}

async fn wait_for_stop(stopping: &AtomicBool) {
    while !stopping.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

struct CaptureHelper(Child);

impl Drop for CaptureHelper {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        for _ in 0..100 {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn has_frame(path: &Path) -> bool {
    let mut header = [0u8; 64];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    let field = |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
    field(0) == 0x4652_5555
        && field(4) == 1
        && field(8) > 0
        && field(12) > 0
        && field(16) >= field(8).saturating_mul(4)
        && field(20) == 1
        && u64::from_le_bytes(header[24..32].try_into().unwrap()) > 0
}

/// Start the native PipeWire consumer with a portal-owned descriptor. The
/// descriptor remains private to the child and is never reopened by path.
pub(crate) fn spawn_helper(pw_fd: OwnedFd, node: u32) -> Result<Child> {
    // The helper inherits this descriptor: strip close-on-exec and keep the
    // OwnedFd alive across the spawn (closing it would invalidate the number
    // the child receives).
    let raw = pw_fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        if flags < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl F_GETFD");
        }
        if libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl F_SETFD");
        }
    }

    let helper = find_helper("uur-pw-capture")?;
    let mut command = Command::new(&helper);
    command
        .env("UUR_PW_FD", raw.to_string())
        .env("UUR_PW_NODE", node.to_string())
        .env("UUR_FRAME_PATH", frame_path()?);
    if let Ok(debug) = std::env::var("UUR_PW_DEBUG") {
        command.env("PIPEWIRE_DEBUG", debug);
    }
    let child = command
        .spawn()
        .with_context(|| format!("running {}", helper.display()))?;
    Ok(child)
}

fn frame_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("UUR_FRAME_PATH") {
        return Ok(PathBuf::from(path));
    }
    Ok(crate::config::runtime_dir()?.join("frames.v1"))
}

/// Open the portal session and return (inherited fd number, node id).
/// First attempt reuses the stored restore token; if that fails (stale
/// token, monitor changed, compositor restart) fall back to one
/// interactive attempt and store the fresh token.
#[allow(clippy::type_complexity)]
async fn portal_session(config: &mut Config) -> Result<(OwnedFd, u32)> {
    let saved_token = config.capture_restore_token.clone();
    let screencast = ashpd::desktop::screencast::Screencast::new()
        .await
        .context("connecting to the ScreenCast portal")?;

    let mut last_error = None;
    let source = source_type(&config.capture_source);
    for token in saved_token
        .clone()
        .map(Some)
        .into_iter()
        .chain(std::iter::once(None))
    {
        match negotiate(&screencast, source, token.as_deref()).await {
            Ok((fd, node, fresh_token)) => {
                if fresh_token != saved_token {
                    config.capture_restore_token = fresh_token;
                    config.store().context("storing the new restore token")?;
                }
                return Ok((fd, node));
            }
            Err(error) => {
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("portal session failed")))
}

/// One full portal handshake.  Returns the PipeWire fd, the video node id
/// and the restore token the portal issued for this selection.
async fn negotiate(
    screencast: &ashpd::desktop::screencast::Screencast<'_>,
    source: ashpd::desktop::screencast::SourceType,
    restore_token: Option<&str>,
) -> Result<(OwnedFd, u32, Option<String>)> {
    use ashpd::desktop::screencast::CursorMode;
    use ashpd::desktop::PersistMode;

    let session = screencast.create_session().await?;

    screencast
        .select_sources(
            &session,
            CursorMode::Embedded,
            source.into(),
            false,
            restore_token,
            PersistMode::ExplicitlyRevoked,
        )
        .await?
        .response()?;

    let response = screencast.start(&session, None).await?;
    let streams_response = response.response()?;
    let streams = streams_response.streams();
    let stream = streams.first().context("no monitor selected")?;
    let node = stream.pipe_wire_node_id();
    let fresh_token = streams_response.restore_token().map(String::from);

    let fd = screencast
        .open_pipe_wire_remote(&session)
        .await
        .context("opening the PipeWire remote")?;
    Ok((fd, node, fresh_token))
}

pub(crate) fn source_type(value: &str) -> ashpd::desktop::screencast::SourceType {
    use ashpd::desktop::screencast::SourceType;
    match value {
        "window" => SourceType::Window,
        "virtual" => SourceType::Virtual,
        _ => SourceType::Monitor,
    }
}

pub fn list_wayland_outputs() -> Result<()> {
    let status = Command::new(find_helper("uur-wl-capture")?)
        .arg("--list-outputs")
        .status()?;
    anyhow::ensure!(status.success(), "could not enumerate Wayland outputs");
    Ok(())
}

fn find_helper(name: &str) -> Result<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/usr/lib/uur").join(name),
        PathBuf::from("/usr/local/lib/uur").join(name),
    ];
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".local/lib/uur").join(name));
    }
    if let Ok(exe) = std::env::current_exe() {
        // dev checkout: target/debug/uur -> capture/uur-pw-capture
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../lib/uur").join(name));
            candidates.push(dir.join("../../capture").join(name));
            candidates.push(dir.join(name));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("capture").join(name));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("capture helper {name} not found (build with capture/build.sh)"))
}

#[cfg(test)]
mod tests {
    use super::source_type;
    use ashpd::desktop::screencast::SourceType;

    #[test]
    fn configured_portal_source_is_mapped() {
        assert_eq!(source_type("monitor"), SourceType::Monitor);
        assert_eq!(source_type("window"), SourceType::Window);
        assert_eq!(source_type("virtual"), SourceType::Virtual);
    }
}
