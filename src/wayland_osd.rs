//! An isolated Wayland OSD process.
//!
//! eframe secondary viewports are tied to the settings window's Wayland event loop. Hyprland
//! can move or pin them, but remapping them across workspaces can still stall compositor pings.
//! This module instead owns one short-lived top-level surface per dictation cycle.

#[cfg(target_os = "linux")]
use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use eframe::egui::{self, Color32, CornerRadius, RichText, Stroke, Vec2, ViewportCommand};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
const TITLE: &str = "auto-voice wayland osd";

#[cfg(target_os = "linux")]
fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("auto-voice-osd.sock")
}

#[cfg(target_os = "linux")]
pub fn show(kind: &str, body: &str) -> Result<()> {
    use std::os::unix::net::UnixDatagram;

    let path = socket_path();
    let socket = UnixDatagram::unbound().context("failed to create OSD client")?;
    let message = format!("{kind}\n{body}");
    if socket.send_to(message.as_bytes(), &path).is_ok() {
        return Ok(());
    }

    if kind != "listening" {
        anyhow::bail!("Wayland OSD is not running");
    }
    let mut agent = std::process::Command::new(std::env::current_exe()?)
        .arg("osd-agent")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to launch Wayland OSD")?;
    // The agent outlives this call by design; reap it whenever it exits so each dictation
    // cycle does not leave a zombie behind.
    std::thread::spawn(move || {
        let _ = agent.wait();
    });
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
enum Phase {
    Listening,
    Processing,
    Done(String),
    Failed(String),
}

#[cfg(target_os = "linux")]
struct Shared {
    phase: Phase,
    changed_at: Instant,
    context: Option<egui::Context>,
}

#[cfg(target_os = "linux")]
pub fn run() -> Result<()> {
    use std::os::unix::net::UnixDatagram;

    let path = socket_path();
    if path.exists() {
        let probe = UnixDatagram::unbound()?;
        if probe.send_to(b"listening\n", &path).is_ok() {
            return Ok(());
        }
        let _ = std::fs::remove_file(&path);
    }
    let socket = UnixDatagram::bind(&path)
        .with_context(|| format!("failed to bind OSD socket {}", path.display()))?;
    let shared = Arc::new(Mutex::new(Shared {
        phase: Phase::Listening,
        changed_at: Instant::now(),
        context: None,
    }));
    let receiver = shared.clone();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(size) = socket.recv(&mut buffer) {
            let message = String::from_utf8_lossy(&buffer[..size]);
            let (kind, body) = message.split_once('\n').unwrap_or((&message, ""));
            let mut state = receiver.lock().unwrap_or_else(|error| error.into_inner());
            state.phase = match kind {
                "listening" => Phase::Listening,
                "processing" => Phase::Processing,
                "done" => Phase::Done(body.to_owned()),
                "failed" => Phase::Failed(body.to_owned()),
                _ => continue,
            };
            state.changed_at = Instant::now();
            if let Some(context) = &state.context {
                context.request_repaint();
            }
        }
    });

    let viewport = egui::ViewportBuilder::default()
        .with_title(TITLE)
        .with_app_id("io.github.billowsand.auto-voice.osd")
        .with_inner_size(Vec2::new(520.0, 116.0))
        .with_min_inner_size(Vec2::new(520.0, 116.0))
        .with_max_inner_size(Vec2::new(520.0, 116.0))
        .with_resizable(false)
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_mouse_passthrough(true)
        .with_taskbar(false)
        .with_active(false);
    let result = eframe::run_native(
        TITLE,
        eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            viewport,
            ..Default::default()
        },
        Box::new(move |creation| {
            let settings = crate::config::ConfigFile::load();
            let system_fonts = crate::platform::list_system_fonts();
            crate::platform::install_ui_fonts(
                &creation.egui_ctx,
                settings.ui_font_families.as_deref(),
                &system_fonts,
            );
            shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .context = Some(creation.egui_ctx.clone());
            Ok(Box::new(Agent { shared }))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Wayland OSD failed: {error}"));
    let _ = std::fs::remove_file(path);
    result
}

#[cfg(target_os = "linux")]
struct Agent {
    shared: Arc<Mutex<Shared>>,
}

#[cfg(target_os = "linux")]
impl eframe::App for Agent {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (phase, changed_at) = {
            let state = self
                .shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            (state.phase.clone(), state.changed_at)
        };
        let (title, body, accent) = match &phase {
            Phase::Listening => (
                "正在聆听",
                "松开右 Alt 后开始转写",
                Color32::from_rgb(78, 203, 172),
            ),
            Phase::Processing => (
                "正在转写",
                "正在识别并插入活动窗口",
                Color32::from_rgb(91, 156, 255),
            ),
            Phase::Done(text) => ("已插入", text.as_str(), Color32::from_rgb(78, 203, 172)),
            Phase::Failed(message) => {
                ("未完成", message.as_str(), Color32::from_rgb(245, 105, 105))
            }
        };

        ui.painter()
            .rect_filled(ui.max_rect(), CornerRadius::ZERO, Color32::TRANSPARENT);
        let card = ui.max_rect().shrink2(Vec2::new(10.0, 10.0));
        ui.painter().rect(
            card,
            CornerRadius::same(18),
            Color32::from_rgba_premultiplied(20, 25, 35, 246),
            Stroke::new(1.0, accent.gamma_multiply(0.7)),
            egui::StrokeKind::Inside,
        );
        let content = card.shrink2(Vec2::new(24.0, 17.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
            ui.horizontal(|ui| {
                let pulse = ((ui.input(|input| input.time) * 4.0).sin() * 0.5 + 0.5) as f32;
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(34.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 8.0 + pulse * 3.0, accent);
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(title)
                            .size(15.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                    ui.label(
                        RichText::new(body)
                            .size(12.5)
                            .color(Color32::from_rgb(174, 184, 201)),
                    );
                });
            });
        });

        if matches!(phase, Phase::Done(_) | Phase::Failed(_))
            && changed_at.elapsed() >= Duration::from_millis(2400)
        {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(32));
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}
