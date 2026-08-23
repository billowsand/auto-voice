//! An isolated Wayland OSD process.
//!
//! eframe secondary viewports are tied to the settings window's Wayland event loop. Hyprland
//! can move or pin them, but remapping them across workspaces can still stall compositor pings.
//! This module instead owns one short-lived top-level surface per dictation cycle.

#[cfg(target_os = "linux")]
use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use eframe::egui::{
    self, Color32, CornerRadius, Pos2, Rect, RichText, Stroke, Vec2, ViewportCommand,
};
#[cfg(target_os = "linux")]
use std::collections::VecDeque;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
const TITLE: &str = "auto-voice wayland osd";

/// Fixed window size. Wayland gives clients no reliable per-application caret position to
/// follow, and letting the compositor's default float placement decide reads as "centres on
/// the cursor" under Hyprland's usual rules — inconsistent enough in practice that a fixed
/// spot is the better default. See [`Agent`]'s positioning in `ui()`.
#[cfg(target_os = "linux")]
const SIZE: Vec2 = Vec2::new(520.0, 116.0);

/// Waveform history depth, matching the desktop OSD's cadence (one slot per ~32ms of audio).
#[cfg(target_os = "linux")]
const WAVE_SLOTS: usize = 26;

#[cfg(target_os = "linux")]
fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("auto-voice-osd.sock")
}

/// Show or update the OSD for a phase change. `kind` is one of `listening`, `processing`,
/// `done`, `copied` or `failed`; the agent is auto-spawned the first time (on `listening`) if it
/// is not already running.
#[cfg(target_os = "linux")]
pub fn show(kind: &str, body: &str) -> Result<()> {
    if send(kind, body) {
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

/// Push one microphone level sample (0.0–1.0) while listening. Fire-and-forget: the agent is
/// expected to already be running from an earlier [`show`] call, and a dropped sample here and
/// there just skips a frame of the waveform rather than breaking anything.
#[cfg(target_os = "linux")]
pub fn send_level(level: f32) {
    send("level", &level.to_string());
}

/// Push an updated running transcript while listening or processing. Same best-effort delivery
/// as [`send_level`].
#[cfg(target_os = "linux")]
pub fn send_partial(text: &str) {
    send("partial", text);
}

/// Send one `kind\nbody` datagram to the agent. Returns whether it was accepted by a listening
/// socket — not whether the agent did anything useful with it.
#[cfg(target_os = "linux")]
fn send(kind: &str, body: &str) -> bool {
    use std::os::unix::net::UnixDatagram;

    let Ok(socket) = UnixDatagram::unbound() else {
        return false;
    };
    let message = format!("{kind}\n{body}");
    socket.send_to(message.as_bytes(), socket_path()).is_ok()
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
enum Phase {
    Listening(String),
    Processing(String),
    Done(String),
    /// No focused window to safely target with a synthetic paste; the text was left on the
    /// clipboard instead.
    CopiedOnly(String),
    Failed(String),
}

#[cfg(target_os = "linux")]
struct Shared {
    phase: Phase,
    changed_at: Instant,
    /// Recent microphone levels, newest last. Only meaningful (and only drawn) while listening.
    levels: VecDeque<f32>,
    last_level_at: Instant,
    /// Running transcript, shown in place of the phase's default hint once non-empty.
    partial: String,
    context: Option<egui::Context>,
}

#[cfg(target_os = "linux")]
impl Shared {
    fn push_level(&mut self, level: f32) {
        // One waveform slot per frame's worth of audio, mirroring the desktop OSD, so the
        // scroll speed reads the same regardless of how the audio arrives from the socket.
        if self.last_level_at.elapsed() >= Duration::from_millis(32) {
            self.last_level_at = Instant::now();
            self.levels.pop_front();
            self.levels.push_back(level);
        } else if let Some(last) = self.levels.back_mut() {
            *last = last.max(level);
        }
    }
}

#[cfg(target_os = "linux")]
pub fn run() -> Result<()> {
    use std::os::unix::net::UnixDatagram;

    let path = socket_path();
    if path.exists() {
        let probe = UnixDatagram::unbound()?;
        if probe.send_to(b"ping\n", &path).is_ok() {
            return Ok(());
        }
        let _ = std::fs::remove_file(&path);
    }
    let socket = UnixDatagram::bind(&path)
        .with_context(|| format!("failed to bind OSD socket {}", path.display()))?;
    let shared = Arc::new(Mutex::new(Shared {
        phase: Phase::Listening(String::new()),
        changed_at: Instant::now(),
        levels: VecDeque::from(vec![0.0; WAVE_SLOTS]),
        last_level_at: Instant::now(),
        partial: String::new(),
        context: None,
    }));
    let receiver = shared.clone();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(size) = socket.recv(&mut buffer) {
            let message = String::from_utf8_lossy(&buffer[..size]);
            let (kind, body) = message.split_once('\n').unwrap_or((&message, ""));
            let mut state = receiver.lock().unwrap_or_else(|error| error.into_inner());

            match kind {
                "level" => {
                    if let Ok(value) = body.trim().parse::<f32>() {
                        state.push_level(value.clamp(0.0, 1.0));
                    }
                    if let Some(context) = &state.context {
                        context.request_repaint();
                    }
                    continue;
                }
                "partial" => {
                    state.partial = body.to_owned();
                    if let Some(context) = &state.context {
                        context.request_repaint();
                    }
                    continue;
                }
                "listening" => {
                    state.levels = VecDeque::from(vec![0.0; WAVE_SLOTS]);
                    state.partial.clear();
                    state.phase = Phase::Listening(body.to_owned());
                }
                "processing" => state.phase = Phase::Processing(body.to_owned()),
                "done" => state.phase = Phase::Done(body.to_owned()),
                "copied" => state.phase = Phase::CopiedOnly(body.to_owned()),
                "failed" => state.phase = Phase::Failed(body.to_owned()),
                _ => continue,
            }
            state.changed_at = Instant::now();
            if let Some(context) = &state.context {
                context.request_repaint();
            }
        }
    });

    let viewport = egui::ViewportBuilder::default()
        .with_title(TITLE)
        .with_app_id("io.github.billowsand.auto-voice.osd")
        .with_inner_size(SIZE)
        .with_min_inner_size(SIZE)
        .with_max_inner_size(SIZE)
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
            Ok(Box::new(Agent {
                shared,
                positioned: false,
                position_attempts: 0,
            }))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Wayland OSD failed: {error}"));
    let _ = std::fs::remove_file(path);
    result
}

#[cfg(target_os = "linux")]
struct Agent {
    shared: Arc<Mutex<Shared>>,
    /// Whether the fixed bottom-center position has been applied yet.
    positioned: bool,
    /// Bounds retries while waiting for the compositor to report a monitor size; without a
    /// cap a compositor that never does would retry forever.
    position_attempts: u8,
}

#[cfg(target_os = "linux")]
impl eframe::App for Agent {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // The compositor's default placement for a new floating, unpositioned window is out
        // of our control — Hyprland's usual rule centres it on the cursor, which reads as
        // "the OSD follows my mouse" and was reported as more distracting than useful. Pin it
        // to a fixed spot instead, the first frame a monitor size is available.
        if !self.positioned && self.position_attempts < 60 {
            self.position_attempts += 1;
            if let Some(monitor) = ui.ctx().input(|input| input.viewport().monitor_size) {
                self.positioned = true;
                let pos = Pos2::new(
                    ((monitor.x - SIZE.x) * 0.5).max(8.0),
                    (monitor.y - SIZE.y - 96.0).max(8.0),
                );
                ui.ctx().send_viewport_cmd(ViewportCommand::OuterPosition(pos));
            }
        }

        let (phase, changed_at, partial, levels) = {
            let state = self
                .shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            (
                state.phase.clone(),
                state.changed_at,
                state.partial.clone(),
                state.levels.iter().copied().collect::<Vec<_>>(),
            )
        };
        let listening = matches!(phase, Phase::Listening(_));
        let (title, hint, accent) = match &phase {
            Phase::Listening(hint) => (
                "正在聆听",
                hint.as_str(),
                Color32::from_rgb(78, 203, 172),
            ),
            Phase::Processing(hint) => (
                "正在整理",
                hint.as_str(),
                Color32::from_rgb(91, 156, 255),
            ),
            Phase::Done(text) => ("已插入", text.as_str(), Color32::from_rgb(78, 203, 172)),
            Phase::CopiedOnly(text) => ("已复制", text.as_str(), Color32::from_rgb(235, 176, 91)),
            Phase::Failed(message) => {
                ("未完成", message.as_str(), Color32::from_rgb(245, 105, 105))
            }
        };
        // While listening or processing, the running transcript takes over the hint line the
        // moment there is one — same rule the desktop OSD uses.
        let body = if matches!(phase, Phase::Listening(_) | Phase::Processing(_)) && !partial.is_empty() {
            partial.as_str()
        } else {
            hint
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

        // A compact waveform in the top-right corner while listening, so Omarchy/Wayland users
        // get the same "what is the microphone actually hearing" feedback Windows/X11 users do
        // instead of just a pulsing dot.
        let text_right_margin = if listening {
            let wave_rect = Rect::from_min_max(
                card.right_top() + Vec2::new(-118.0, 16.0),
                card.right_top() + Vec2::new(-22.0, 42.0),
            );
            crate::osd::draw_waveform(ui.painter(), wave_rect, &levels, accent.gamma_multiply(0.85));
            104.0
        } else {
            0.0
        };

        let content = card.shrink2(Vec2::new(24.0, 17.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
            ui.horizontal(|ui| {
                let pulse = ((ui.input(|input| input.time) * 4.0).sin() * 0.5 + 0.5) as f32;
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(34.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 8.0 + pulse * 3.0, accent);
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.set_max_width((content.width() - 44.0 - text_right_margin).max(60.0));
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

        if matches!(phase, Phase::Done(_) | Phase::CopiedOnly(_) | Phase::Failed(_))
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
