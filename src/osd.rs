//! The push-to-talk overlay: a floating pill that pops next to the caret while the hotkey is
//! held, follows the voice with a live waveform, shows what was recognised, and fades away.
//!
//! The speech pipeline talks to [`OsdHandle`]. Rendering and native-window details stay
//! on the desktop UI thread. Nothing is painted unless a dictation is in flight — the native
//! surface is kept alive on Windows only because re-showing a window there steals focus from
//! the app receiving the text.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2,
    ViewportBuilder, ViewportCommand, ViewportId,
};

/// The native surface is a transparent canvas; the pill is drawn centred inside it so it can
/// grow and shrink without resizing (and re-compositing) a window on every phase change.
const CANVAS: Vec2 = Vec2::new(620.0, 112.0);
/// Must match `ViewportBuilder::with_title` below: the color-key hookup finds the native
/// overlay window by title.
const OVERLAY_TITLE: &str = "auto-voice status";
const PILL_HEIGHT: f32 = 62.0;
const WAVE_SLOTS: usize = 44;

/// Where the overlay waits between dictations on platforms that keep it mapped.
const PARKED: Pos2 = Pos2::new(-20_000.0, -20_000.0);

const FADE: f32 = 0.14;
const FADE_OUT: Duration = Duration::from_millis(180);
const DONE_VISIBLE_FOR: Duration = Duration::from_millis(1500);
const NOTICE_VISIBLE_FOR: Duration = Duration::from_millis(2600);

pub fn viewport_id() -> ViewportId {
    ViewportId::from_hash_of("auto-voice-osd")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OsdPhase {
    Hidden,
    /// Hotkey held, microphone open.
    Listening,
    /// Hotkey released, ASR + polish running.
    Processing,
    /// Text was inserted; the pill shows it for a moment.
    Done,
    /// Nothing usable to insert, or something went wrong.
    Notice,
}

#[derive(Clone, Debug)]
pub struct OsdSnapshot {
    pub phase: OsdPhase,
    pub level: f32,
    pub elapsed: Duration,
    pub changed_at: Instant,
    pub text: String,
    pub warn: bool,
    pub hotkey: String,
    pub levels: Vec<f32>,
}

struct OsdState {
    phase: OsdPhase,
    level: f32,
    levels: VecDeque<f32>,
    last_level_at: Instant,
    recording_started: Option<Instant>,
    elapsed: Duration,
    changed_at: Instant,
    hidden_at: Instant,
    text: String,
    warn: bool,
    hotkey: String,
    follow_caret: bool,
    context: Option<egui::Context>,
}

impl Default for OsdState {
    fn default() -> Self {
        Self {
            phase: OsdPhase::Hidden,
            level: 0.0,
            levels: VecDeque::from(vec![0.0; WAVE_SLOTS]),
            last_level_at: Instant::now(),
            recording_started: None,
            elapsed: Duration::ZERO,
            changed_at: Instant::now(),
            hidden_at: Instant::now() - FADE_OUT,
            text: String::new(),
            warn: false,
            hotkey: "Caps Lock".to_owned(),
            follow_caret: true,
            context: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct OsdHandle {
    state: Arc<Mutex<OsdState>>,
}

impl OsdHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach_context(&self, context: &egui::Context) {
        let mut state = self.lock_state();
        state.context = Some(context.clone());
        let visible = surface_visible(&state);
        drop(state);
        context.send_viewport_cmd_to(viewport_id(), ViewportCommand::Visible(visible));
        context.request_repaint_of(viewport_id());
    }

    /// Label shown under the title while listening, e.g. `"Caps Lock"`.
    pub fn set_hotkey_label(&self, label: impl Into<String>) {
        self.lock_state().hotkey = label.into();
    }

    pub fn set_follow_caret(&self, follow: bool) {
        self.lock_state().follow_caret = follow;
    }

    pub fn set_recording(&self) {
        let now = Instant::now();
        // Read the focused window before we pop, so the anchor is the app being dictated into.
        let follow = self.lock_state().follow_caret;
        let anchor = crate::platform::overlay_origin(CANVAS, follow);
        self.update(|state| {
            state.phase = OsdPhase::Listening;
            state.level = 0.0;
            state.levels.iter_mut().for_each(|slot| *slot = 0.0);
            state.recording_started = Some(now);
            state.elapsed = Duration::ZERO;
            state.changed_at = now;
            state.text.clear();
            state.warn = false;
        });
        self.move_to(anchor);
    }

    pub fn set_processing(&self) {
        let now = Instant::now();
        self.update(|state| {
            if let Some(started) = state.recording_started.take() {
                state.elapsed = now.saturating_duration_since(started);
            }
            state.phase = OsdPhase::Processing;
            state.level = 0.0;
            state.changed_at = now;
        });
    }

    /// Text made it into the focused app.
    pub fn set_done(&self, text: &str) {
        let now = Instant::now();
        let text = text.trim().to_owned();
        self.update(|state| {
            if let Some(started) = state.recording_started.take() {
                state.elapsed = now.saturating_duration_since(started);
            }
            state.phase = OsdPhase::Done;
            state.level = 0.0;
            state.changed_at = now;
            state.text = text;
            state.warn = false;
        });
    }

    /// Nothing was inserted. `warn` separates real failures from "didn't catch that".
    pub fn set_notice(&self, message: impl Into<String>, warn: bool) {
        let now = Instant::now();
        let message = message.into();
        self.update(|state| {
            state.recording_started = None;
            state.phase = OsdPhase::Notice;
            state.level = 0.0;
            state.changed_at = now;
            state.text = message;
            state.warn = warn;
        });
    }

    pub fn hide(&self) {
        self.update(|state| {
            state.phase = OsdPhase::Hidden;
            state.level = 0.0;
            state.recording_started = None;
            state.elapsed = Duration::ZERO;
            state.changed_at = Instant::now();
            state.hidden_at = Instant::now();
            state.text.clear();
        });
    }

    /// New recordings are blocked until the previous one finished showing its result.
    pub fn can_recording_start(&self) -> bool {
        self.lock_state().phase == OsdPhase::Hidden
    }

    pub fn set_level(&self, level: f32) {
        let mut state = self.lock_state();
        if state.phase != OsdPhase::Listening {
            return;
        }
        let level = level.clamp(0.0, 1.0);
        state.level = level;
        // One waveform slot per frame's worth of audio keeps the scroll speed readable
        // regardless of the capture buffer size.
        if state.last_level_at.elapsed() >= Duration::from_millis(32) {
            state.last_level_at = Instant::now();
            state.levels.pop_front();
            state.levels.push_back(level);
        } else if let Some(last) = state.levels.back_mut() {
            *last = last.max(level);
        }
        if let Some(context) = &state.context {
            context.request_repaint_of(viewport_id());
        }
    }

    pub fn snapshot(&self) -> OsdSnapshot {
        let state = self.lock_state();
        let elapsed = state
            .recording_started
            .map_or(state.elapsed, |started| started.elapsed());
        OsdSnapshot {
            phase: state.phase,
            level: state.level,
            elapsed,
            changed_at: state.changed_at,
            text: state.text.clone(),
            warn: state.warn,
            hotkey: state.hotkey.clone(),
            levels: state.levels.iter().copied().collect(),
        }
    }

    pub fn native_surface_visible(&self) -> bool {
        surface_visible(&self.lock_state())
    }

    /// Retire a result pill once it has been on screen long enough.
    pub fn tick(&self) {
        let snapshot = self.snapshot();
        let linger = match snapshot.phase {
            OsdPhase::Done => visible_for(&snapshot.text),
            OsdPhase::Notice => NOTICE_VISIBLE_FOR,
            _ => return,
        };
        if snapshot.changed_at.elapsed() >= linger {
            self.hide();
        }
    }

    fn move_to(&self, anchor: Option<Pos2>) {
        let Some(anchor) = anchor else { return };
        let Some(context) = self.lock_state().context.clone() else {
            return;
        };
        let points = context.pixels_per_point().max(0.1);
        context.send_viewport_cmd_to(
            viewport_id(),
            ViewportCommand::OuterPosition(Pos2::new(anchor.x / points, anchor.y / points)),
        );
    }

    fn update(&self, mutate: impl FnOnce(&mut OsdState)) {
        let mut state = self.lock_state();
        mutate(&mut state);
        let context = state.context.clone();
        let visible = surface_visible(&state);
        drop(state);

        if let Some(context) = context {
            // Only ever shown from here. Hiding waits for the fade-out to finish in `draw`.
            if visible {
                context.send_viewport_cmd_to(viewport_id(), ViewportCommand::Visible(true));
            }
            context.request_repaint_of(viewport_id());
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, OsdState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Longer results deserve a longer look, but never long enough to feel in the way.
fn visible_for(text: &str) -> Duration {
    let extra = (text.chars().count() as u64).min(60) * 26;
    DONE_VISIBLE_FOR + Duration::from_millis(extra)
}

fn surface_visible(state: &OsdState) -> bool {
    // Winit uses SW_SHOWNOACTIVATE only for the first show on Windows and SW_SHOW for later
    // visibility cycles. Keeping a transparent, click-through surface alive prevents the OSD
    // from stealing focus from the application receiving dictated text.
    cfg!(target_os = "windows")
        || state.phase != OsdPhase::Hidden
        || state.hidden_at.elapsed() < FADE_OUT
}

pub fn viewport_builder(monitor_size: Option<Vec2>, visible: bool) -> ViewportBuilder {
    let mut builder = ViewportBuilder::default()
        .with_title(OVERLAY_TITLE)
        .with_inner_size(CANVAS)
        .with_min_inner_size(CANVAS)
        .with_max_inner_size(CANVAS)
        .with_resizable(false)
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_mouse_passthrough(true)
        .with_taskbar(false)
        .with_active(false)
        .with_visible(visible);

    if let Some(monitor) = monitor_size {
        builder = builder.with_position(crate::platform::overlay_fallback_origin(CANVAS, monitor));
    }
    builder
}

// ── Painting ─────────────────────────────────────────────────────────────────

pub fn draw(ui: &mut egui::Ui, handle: &OsdHandle) {
    punch_out_background_once();
    handle.tick();
    let snapshot = handle.snapshot();
    let open = snapshot.phase != OsdPhase::Hidden;

    // One eased value drives fade, lift and scale, so opening and closing mirror each other.
    let progress = ui
        .ctx()
        .animate_bool_with_time(Id::new("auto-voice-osd-open"), open, FADE);
    if progress <= 0.002 {
        if !open {
            // Windows keeps the surface mapped (re-showing it would steal focus from the app
            // being dictated into), so park it off-screen instead: even if the compositor or
            // the GL config refuses alpha, there is nothing left to see.
            let command = if cfg!(target_os = "windows") {
                ViewportCommand::OuterPosition(PARKED)
            } else {
                ViewportCommand::Visible(false)
            };
            ui.ctx().send_viewport_cmd_to(viewport_id(), command);
        }
        return;
    }

    let palette = Palette::for_snapshot(&snapshot);
    let width = pill_width(ui, &snapshot);
    let canvas = ui.max_rect();
    let scale = 0.96 + 0.04 * progress;
    let pill = Rect::from_center_size(
        canvas.center() + Vec2::new(0.0, (1.0 - progress) * 10.0),
        Vec2::new(width, PILL_HEIGHT) * scale,
    );
    let painter = ui.painter();
    let alpha = |color: Color32| color.gamma_multiply(progress);

    painter.rect_filled(
        pill.translate(Vec2::new(0.0, 6.0)).expand(2.0),
        CornerRadius::same(24),
        alpha(Color32::from_black_alpha(60)),
    );
    painter.rect_filled(pill, CornerRadius::same(22), alpha(palette.surface));
    painter.rect_stroke(
        pill,
        CornerRadius::same(22),
        Stroke::new(1.0, alpha(palette.border)),
        StrokeKind::Inside,
    );

    let orb = Pos2::new(pill.left() + 34.0, pill.center().y);
    draw_orb(painter, orb, &snapshot, palette.accent, progress);

    let text_left = pill.left() + 62.0;
    match snapshot.phase {
        OsdPhase::Listening => {
            let tail = pill.right() - 16.0;
            let timer = format!("{:.1}s", snapshot.elapsed.as_secs_f32());
            let timer_width = 42.0;
            painter.text(
                Pos2::new(tail, pill.center().y),
                Align2::RIGHT_CENTER,
                timer,
                FontId::proportional(13.0),
                alpha(palette.muted),
            );
            draw_waveform(
                painter,
                Rect::from_min_max(
                    Pos2::new(pill.right() - 150.0 - timer_width, pill.top() + 14.0),
                    Pos2::new(tail - timer_width - 8.0, pill.bottom() - 14.0),
                ),
                &snapshot.levels,
                alpha(palette.accent),
            );
            two_line(
                painter,
                text_left,
                pill,
                "正在聆听",
                &format!("松开 {} 插入文字", snapshot.hotkey),
                &palette,
                progress,
            );
        }
        OsdPhase::Processing => {
            two_line(
                painter,
                text_left,
                pill,
                "正在转写",
                &format!("本地识别中 · {:.1}s 语音", snapshot.elapsed.as_secs_f32()),
                &palette,
                progress,
            );
            draw_progress_track(
                painter,
                Rect::from_min_max(
                    Pos2::new(pill.right() - 96.0, pill.center().y - 2.5),
                    Pos2::new(pill.right() - 20.0, pill.center().y + 2.5),
                ),
                snapshot.changed_at.elapsed(),
                alpha(palette.accent),
            );
        }
        OsdPhase::Done | OsdPhase::Notice => {
            let (title, body) = if snapshot.phase == OsdPhase::Done {
                ("已插入", snapshot.text.as_str())
            } else {
                ("未插入", snapshot.text.as_str())
            };
            let galley = result_galley(ui, body, pill.right() - text_left - 18.0, palette.text);
            let painter = ui.painter();
            painter.text(
                Pos2::new(text_left, pill.top() + 13.0),
                Align2::LEFT_TOP,
                title,
                FontId::proportional(11.5),
                alpha(palette.muted),
            );
            painter.galley(
                Pos2::new(text_left, pill.top() + 29.0),
                galley,
                alpha(palette.text),
            );
        }
        OsdPhase::Hidden => {}
    }

    match snapshot.phase {
        OsdPhase::Listening | OsdPhase::Processing => {
            ui.ctx()
                .request_repaint_after_for(Duration::from_millis(33), viewport_id());
        }
        OsdPhase::Done | OsdPhase::Notice => {
            let linger = if snapshot.phase == OsdPhase::Done {
                visible_for(&snapshot.text)
            } else {
                NOTICE_VISIBLE_FOR
            };
            ui.ctx().request_repaint_after_for(
                linger.saturating_sub(snapshot.changed_at.elapsed()),
                viewport_id(),
            );
        }
        // Keep repainting through the fade-out so the surface can be released afterwards.
        OsdPhase::Hidden => ui
            .ctx()
            .request_repaint_after_for(Duration::from_millis(33), viewport_id()),
    }
}

/// The native window only exists once the viewport has been created, so the color key is
/// applied from the first paint rather than at startup.
fn punch_out_background_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    if crate::platform::punch_out_overlay_background(OVERLAY_TITLE) {
        DONE.store(true, Ordering::Relaxed);
    }
}

fn pill_width(ui: &egui::Ui, snapshot: &OsdSnapshot) -> f32 {
    match snapshot.phase {
        OsdPhase::Listening => 344.0,
        OsdPhase::Processing => 316.0,
        _ => {
            let measured = ui
                .painter()
                .layout_no_wrap(
                    snapshot.text.clone(),
                    FontId::proportional(14.5),
                    Color32::WHITE,
                )
                .size()
                .x;
            (measured + 92.0).clamp(240.0, CANVAS.x - 40.0)
        }
    }
}

fn result_galley(
    ui: &egui::Ui,
    text: &str,
    max_width: f32,
    color: Color32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        FontId::proportional(14.5),
        color,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    ui.painter().layout_job(job)
}

fn two_line(
    painter: &egui::Painter,
    left: f32,
    pill: Rect,
    title: &str,
    subtitle: &str,
    palette: &Palette,
    progress: f32,
) {
    painter.text(
        Pos2::new(left, pill.top() + 12.0),
        Align2::LEFT_TOP,
        title,
        FontId::proportional(15.5),
        palette.text.gamma_multiply(progress),
    );
    painter.text(
        Pos2::new(left, pill.top() + 34.0),
        Align2::LEFT_TOP,
        subtitle,
        FontId::proportional(11.5),
        palette.muted.gamma_multiply(progress),
    );
}

/// The status orb: a breathing ring while listening, an orbiting arc while transcribing,
/// a tick or an exclamation once it is over.
fn draw_orb(
    painter: &egui::Painter,
    center: Pos2,
    snapshot: &OsdSnapshot,
    accent: Color32,
    progress: f32,
) {
    let accent = accent.gamma_multiply(progress);
    match snapshot.phase {
        OsdPhase::Listening => {
            let pulse = 15.0 + snapshot.level * 7.0;
            painter.circle_filled(center, pulse, accent.gamma_multiply(0.16));
            painter.circle_filled(center, 6.0 + snapshot.level * 2.5, accent);
        }
        OsdPhase::Processing => {
            painter.circle_filled(center, 15.0, accent.gamma_multiply(0.16));
            draw_arc(
                painter,
                center,
                11.0,
                snapshot.changed_at.elapsed().as_secs_f32() * 4.2,
                accent,
            );
        }
        OsdPhase::Done => {
            painter.circle_filled(center, 15.0, accent.gamma_multiply(0.18));
            let stroke = Stroke::new(2.4, accent);
            painter.line_segment(
                [center + Vec2::new(-6.0, 0.0), center + Vec2::new(-1.5, 4.6)],
                stroke,
            );
            painter.line_segment(
                [center + Vec2::new(-1.5, 4.6), center + Vec2::new(6.6, -4.8)],
                stroke,
            );
        }
        OsdPhase::Notice => {
            painter.circle_filled(center, 15.0, accent.gamma_multiply(0.18));
            painter.line_segment(
                [center + Vec2::new(0.0, -6.0), center + Vec2::new(0.0, 2.0)],
                Stroke::new(2.4, accent),
            );
            painter.circle_filled(center + Vec2::new(0.0, 6.0), 1.5, accent);
        }
        OsdPhase::Hidden => {}
    }
}

fn draw_arc(painter: &egui::Painter, center: Pos2, radius: f32, phase: f32, color: Color32) {
    let points: Vec<Pos2> = (0..=18)
        .map(|step| {
            let angle = phase + step as f32 * (std::f32::consts::TAU * 0.42 / 18.0);
            center + Vec2::angled(angle) * radius
        })
        .collect();
    painter.add(Shape::line(points, Stroke::new(2.4, color)));
}

/// Scrolling history of the microphone level: what was actually heard, not a canned animation.
fn draw_waveform(painter: &egui::Painter, rect: Rect, levels: &[f32], color: Color32) {
    if rect.width() <= 0.0 || levels.is_empty() {
        return;
    }
    let step = rect.width() / levels.len() as f32;
    let center_y = rect.center().y;
    for (index, level) in levels.iter().enumerate() {
        let x = rect.left() + step * (index as f32 + 0.5);
        // Newer samples on the right are drawn brighter, so the bar reads as moving.
        let recency = 0.35 + 0.65 * (index as f32 / levels.len() as f32);
        let height = (3.0 + level.sqrt() * rect.height() * 0.9).min(rect.height());
        painter.rect_filled(
            Rect::from_center_size(Pos2::new(x, center_y), Vec2::new(2.6, height)),
            CornerRadius::same(2),
            color.gamma_multiply(recency),
        );
    }
}

fn draw_progress_track(painter: &egui::Painter, track: Rect, elapsed: Duration, color: Color32) {
    painter.rect_filled(track, CornerRadius::same(3), color.gamma_multiply(0.18));
    // Indeterminate: a shuttle sweeping the track, since ASR gives us no progress to report.
    let cycle = (elapsed.as_secs_f32() * 0.9).fract();
    let eased = 0.5 - 0.5 * (cycle * std::f32::consts::TAU).cos();
    let width = track.width() * 0.42;
    let left = track.left() + (track.width() - width) * eased;
    painter.rect_filled(
        Rect::from_min_size(
            Pos2::new(left, track.top()),
            Vec2::new(width, track.height()),
        ),
        CornerRadius::same(3),
        color,
    );
}

struct Palette {
    surface: Color32,
    border: Color32,
    accent: Color32,
    text: Color32,
    muted: Color32,
}

impl Palette {
    fn for_snapshot(snapshot: &OsdSnapshot) -> Self {
        let accent = match snapshot.phase {
            OsdPhase::Hidden => Color32::from_rgb(112, 126, 151),
            OsdPhase::Listening => Color32::from_rgb(255, 92, 116),
            OsdPhase::Processing => Color32::from_rgb(104, 156, 255),
            OsdPhase::Done => Color32::from_rgb(67, 211, 151),
            OsdPhase::Notice if snapshot.warn => Color32::from_rgb(255, 122, 122),
            OsdPhase::Notice => Color32::from_rgb(235, 176, 91),
        };
        Self {
            surface: Color32::from_rgba_premultiplied(19, 24, 34, 246),
            border: accent.gamma_multiply(0.42),
            accent,
            text: Color32::from_rgb(245, 247, 250),
            muted: Color32::from_rgb(151, 162, 180),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_lifecycle_blocks_reentry_until_hidden() {
        let handle = OsdHandle::new();
        assert!(handle.can_recording_start());
        handle.set_recording();
        assert!(!handle.can_recording_start());
        handle.set_processing();
        handle.set_done("你好");
        assert!(!handle.can_recording_start());
        handle.hide();
        assert!(handle.can_recording_start());
    }

    #[test]
    fn audio_level_is_clamped_and_recorded_in_the_waveform() {
        let handle = OsdHandle::new();
        handle.set_recording();
        handle.set_level(2.0);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.level, 1.0);
        assert_eq!(snapshot.levels.len(), WAVE_SLOTS);
        assert_eq!(snapshot.levels.last().copied(), Some(1.0));
    }

    #[test]
    fn a_result_carries_the_inserted_text() {
        let handle = OsdHandle::new();
        handle.set_recording();
        handle.set_processing();
        handle.set_done("  今天天气不错  ");
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.phase, OsdPhase::Done);
        assert_eq!(snapshot.text, "今天天气不错");
    }

    #[test]
    fn longer_results_stay_on_screen_longer() {
        assert!(visible_for("好") < visible_for(&"好".repeat(30)));
    }
}
