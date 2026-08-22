//! Reliable compositor-to-process PTT control for Wayland.
//!
//! Unix signals are unsuitable here: their default action can terminate the application before
//! a handler is installed, they target every matching process when sent through `pkill`, and
//! they carry no instance-safe state. A Unix datagram socket gives us explicit press/release
//! messages and harmless failure when Auto Voice is not running.

#[cfg(target_os = "linux")]
use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("auto-voice-ptt.sock")
}

#[cfg(target_os = "linux")]
pub struct Server {
    socket_path: PathBuf,
}

#[cfg(target_os = "linux")]
pub fn ensure_no_running_instance() -> Result<()> {
    use std::os::unix::net::UnixDatagram;

    let path = socket_path();
    if !path.exists() {
        return Ok(());
    }
    let probe = UnixDatagram::unbound().context("failed to probe Auto Voice instance")?;
    if probe.send_to(b"ping", &path).is_ok() {
        anyhow::bail!("Auto Voice is already running");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
impl Server {
    pub fn start(held: Arc<AtomicBool>) -> Result<Self> {
        use std::os::unix::net::UnixDatagram;

        let socket_path = socket_path();
        if socket_path.exists() {
            let probe = UnixDatagram::unbound().context("failed to probe PTT socket")?;
            if probe.send_to(b"ping", &socket_path).is_ok() {
                anyhow::bail!("Auto Voice is already running");
            }
            std::fs::remove_file(&socket_path).with_context(|| {
                format!(
                    "failed to remove stale PTT socket {}",
                    socket_path.display()
                )
            })?;
        }
        let socket = UnixDatagram::bind(&socket_path)
            .with_context(|| format!("failed to bind PTT socket {}", socket_path.display()))?;

        std::thread::spawn(move || {
            let mut message = [0_u8; 16];
            loop {
                match socket.recv(&mut message) {
                    Ok(size) => match &message[..size] {
                        b"press" => held.store(true, Ordering::SeqCst),
                        b"release" => held.store(false, Ordering::SeqCst),
                        b"ping" => {}
                        _ => tracing::warn!("Ignoring unknown Wayland PTT message"),
                    },
                    Err(error) => {
                        tracing::warn!("Wayland PTT socket stopped: {error}");
                        break;
                    }
                }
            }
        });

        tracing::info!("Wayland PTT IPC ready at {}", socket_path.display());
        Ok(Self { socket_path })
    }
}

#[cfg(target_os = "linux")]
impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(target_os = "linux")]
pub fn send(message: &[u8]) -> Result<()> {
    use std::os::unix::net::UnixDatagram;

    let endpoint = socket_path();
    let socket = UnixDatagram::unbound().context("failed to create PTT client socket")?;
    socket
        .send_to(message, &endpoint)
        .with_context(|| format!("failed to contact Auto Voice at {}", endpoint.display()))?;
    Ok(())
}
