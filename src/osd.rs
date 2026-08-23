//! The push-to-talk overlay: a floating card that pops next to the caret while the hotkey is
//! held, follows the voice with a live waveform, streams the transcript in as it is recognised,
//! and fades away once the text has landed.
//!
//! The speech pipeline talks to [`OsdHandle`]. Rendering and native-window details stay
//! on the desktop UI thread. Nothing is painted unless a dictation is in flight — the native
//! surface is kept alive on Windows only because re-showing a window there steals focus from
//! the app receiving the text.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eframe::egui::epaint::{RectShape, Shadow};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2,
    ViewportBuilder, ViewportCommand, ViewportId,
};

use crate::platform::OverlayCompositing;

/// The native surface is a transparent canvas; the card is drawn inside it so it can grow and
/// shrink without resizing (and re-compositing) a window on every phase change. It has to be
/// large enough for the widest card plus the shadow that falls outside it.
const CANVAS: Vec2 = Vec2::new(760.0, 260.0);
/// Must match `ViewportBuilder::with_title` below: the compositing hookup finds the native
/// overlay window by title.
const OVERLAY_TITLE: &str = "auto-voice status";

/// Where the card sits inside the canvas. The caret anchor is corrected by this, so the card —
/// not the invisible canvas around it — is what lands under the insertion point.
const CARD_TOP: f32 = 26.0;
const CARD_WIDTH_LIVE: f32 = 470.0;
const CARD_WIDTH_MIN: f32 = 340.0;
const CARD_WIDTH_MAX: f32 = 600.0;

/// Header = status orb, title and the meter. The transcript grows below it.
const HEADER_HEIGHT: f32 = 48.0;
const BODY_LEFT: f32 = 62.0;
const BODY_RIGHT: f32 = 20.0;
const BODY_BOTTOM: f32 = 16.0;
const BODY_FONT: f32 = 15.0;
const TITLE_FONT: f32 = 13.0;
/// The transcript scrolls: only the newest lines stay on screen.
const BODY_MAX_ROWS: usize = 3;

const WAVE_SLOTS: usize = 30;
const WAVE_WIDTH: f32 = 118.0;
const TIMER_WIDTH: f32 = 40.0;

/// Where the overlay waits between dictations on platforms that keep it mapped.
const PARKED: Pos2 = Pos2::new(-20_000.0, -20_000.0);

const FADE: f32 = 0.14;
/// Width and height ease between phases instead of snapping, so the card feels like one object
/// that grows around the text rather than a series of different pills.
const RESIZE: f32 = 0.13;
const FADE_OUT: Duration = Duration::from_millis(180);
const DONE_VISIBLE_FOR: Duration = Duration::from_millis(1500);
const NOTICE_VISIBLE_FOR: Duration = Duration::from_millis(2600);

pub fn viewport_id() -> ViewportId {
    ViewportId::from_hash_of("auto-voice-osd")
}

/// Offset from the overlay window's top-left corner to the top-left corner of the card, for the
/// width the card has while listening. Anchoring uses this so the caret ends up beside the card.
pub fn card_inset() -> Vec2 {
    Vec2::new((CANVAS.x - CARD_WIDTH_LIVE) * 0.5, CARD_TOP)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OsdPhase {
    Hidden,
    /// Hotkey held, microphone open.
    Listening,
    /// Hotkey released, ASR + polish running.
    Processing,
    /// Text was inserted; the card shows it for a moment.
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
    /// What has been recognised so far this dictation. Still subject to change.
    pub partial: String,
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
    partial: String,
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
            partial: String::new(),
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
        let anchor = crate::platform::overlay_origin(CANVAS, card_inset(), follow);
        self.update(|state| {
            state.phase = OsdPhase::Listening;
            state.level = 0.0;
            state.levels.iter_mut().for_each(|slot| *slot = 0.0);
            state.recording_started = Some(now);
            state.elapsed = Duration::ZERO;
            state.changed_at = now;
            state.text.clear();
            state.partial.clear();
            state.warn = false;
        });
        self.move_to(anchor);
    }

    /// Everything recognised so far. Shown as-is, and replaced wholesale by the next update:
    /// the recogniser is free to revise words it has already emitted.
    pub fn set_partial(&self, text: &str) {
        let text = text.trim();
        let mut state = self.lock_state();
        if !matches!(state.phase, OsdPhase::Listening | OsdPhase::Processing) {
            return;
        }
        if state.partial == text {
            return;
        }
        state.partial = text.to_owned();
        if let Some(context) = &state.context {
            context.request_repaint_of(viewport_id());
        }
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
            state.partial.clear();
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
            state.partial.clear();
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
            state.partial.clear();
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
            partial: state.partial.clone(),
            warn: state.warn,
            hotkey: state.hotkey.clone(),
            levels: state.levels.iter().copied().collect(),
        }
    }

    pub fn native_surface_visible(&self) -> bool {
        surface_visible(&self.lock_state())
    }

    /// Retire a result card once it has been on screen long enough.
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
            // On non-Windows platforms the hidden overlay viewport is not created at startup.
            // Repaint the root viewport so DesktopApp can create it on first use.
            context.request_repaint();
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
    let compositing = overlay_compositing();
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

    let palette = Palette::for_snapshot(&snapshot, compositing);
    let content = Content::for_snapshot(&snapshot);
    let alpha = |color: Color32| color.gamma_multiply(progress);

    // While the card is still opening its size is snapped rather than eased: the tween is there
    // to absorb changes in an on-screen card, not to inflate it on the way in.
    let settling = progress > 0.98;
    let width = animate(
        ui,
        "auto-voice-osd-width",
        content.target_width(ui),
        settling,
    );
    let body = body_galley(
        ui,
        &content.body,
        width - BODY_LEFT - BODY_RIGHT,
        alpha(content.body_color(&palette)),
    );
    let height = animate(
        ui,
        "auto-voice-osd-height",
        HEADER_HEIGHT + body.size().y + BODY_BOTTOM,
        settling,
    );

    let canvas = ui.max_rect();
    let card = Rect::from_min_size(
        Pos2::new(
            canvas.left() + (canvas.width() - width) * 0.5,
            canvas.top() + CARD_TOP + (1.0 - progress) * 8.0,
        ),
        Vec2::new(width, height),
    );
    let radius = card_radius(height);

    draw_surface(ui.painter(), card, radius, &palette, progress, compositing);

    let header_y = card.top() + HEADER_HEIGHT * 0.5;
    draw_orb(
        ui.painter(),
        Pos2::new(card.left() + 33.0, header_y),
        &snapshot,
        alpha(palette.accent),
    );
    ui.painter().text(
        Pos2::new(card.left() + BODY_LEFT, header_y),
        Align2::LEFT_CENTER,
        content.title,
        FontId::proportional(TITLE_FONT),
        alpha(palette.title),
    );
    draw_meter(
        ui.painter(),
        card,
        header_y,
        &snapshot,
        &content,
        &palette,
        progress,
    );

    ui.painter().galley(
        Pos2::new(card.left() + BODY_LEFT, card.top() + HEADER_HEIGHT),
        body,
        alpha(content.body_color(&palette)),
    );

    schedule_repaint(ui.ctx(), &snapshot);
}

fn schedule_repaint(ctx: &egui::Context, snapshot: &OsdSnapshot) {
    match snapshot.phase {
        OsdPhase::Listening | OsdPhase::Processing => {
            ctx.request_repaint_after_for(Duration::from_millis(33), viewport_id());
        }
        OsdPhase::Done | OsdPhase::Notice => {
            let linger = if snapshot.phase == OsdPhase::Done {
                visible_for(&snapshot.text)
            } else {
                NOTICE_VISIBLE_FOR
            };
            ctx.request_repaint_after_for(
                linger.saturating_sub(snapshot.changed_at.elapsed()),
                viewport_id(),
            );
        }
        // Keep repainting through the fade-out so the surface can be released afterwards.
        OsdPhase::Hidden => ctx.request_repaint_after_for(Duration::from_millis(33), viewport_id()),
    }
}

/// The compositing mode is discovered once, from the first frame: the native window only exists
/// after the viewport has been created, so this cannot be settled at startup.
fn overlay_compositing() -> OverlayCompositing {
    const UNKNOWN: u8 = 0;
    const ALPHA: u8 = 1;
    const COLOR_KEY: u8 = 2;
    static MODE: AtomicU8 = AtomicU8::new(UNKNOWN);

    match MODE.load(Ordering::Relaxed) {
        ALPHA => OverlayCompositing::PerPixelAlpha,
        COLOR_KEY => OverlayCompositing::ColorKey,
        _ => match crate::platform::prepare_overlay_window(OVERLAY_TITLE) {
            Some(mode) => {
                tracing::info!("Overlay compositing: {mode:?}");
                MODE.store(
                    match mode {
                        OverlayCompositing::PerPixelAlpha => ALPHA,
                        OverlayCompositing::ColorKey => COLOR_KEY,
                    },
                    Ordering::Relaxed,
                );
                mode
            }
            // The window is not up yet; assume the good path for this one frame.
            None => OverlayCompositing::PerPixelAlpha,
        },
    }
}

fn animate(ui: &egui::Ui, id: &str, target: f32, settling: bool) -> f32 {
    ui.ctx()
        .animate_value_with_time(Id::new(id), target, if settling { RESIZE } else { 0.0 })
}

/// A capsule while the card is one line tall, a rounded card once the transcript wraps.
fn card_radius(height: f32) -> CornerRadius {
    CornerRadius::same((height * 0.5).min(26.0).round() as u8)
}

/// Glass: a translucent slab, a light falling across the top of it, and a rim that picks up the
/// accent colour of the current phase.
///
/// Under a colour key none of that is available — partial alpha would composite as a black
/// fringe — so the same card is painted opaque, without a shadow.
fn draw_surface(
    painter: &egui::Painter,
    card: Rect,
    radius: CornerRadius,
    palette: &Palette,
    progress: f32,
    compositing: OverlayCompositing,
) {
    let alpha = |color: Color32| color.gamma_multiply(progress);

    if compositing.blends() {
        let shadow = Shadow {
            offset: [0, 10],
            blur: 30,
            spread: 0,
            color: alpha(Color32::from_black_alpha(96)),
        };
        painter.add(shadow.as_shape(card, radius));
    }

    painter.add(
        RectShape::filled(card, radius, alpha(palette.surface))
            .with_stroke_kind(StrokeKind::Inside)
            .with_round_to_pixels(false),
    );

    if compositing.blends() {
        // A soft pool of light in the upper half. Blurred rather than clipped, so it reads as a
        // gradient instead of a second rectangle sitting on the card.
        let sheen = Rect::from_min_max(
            card.min + Vec2::new(15.0, 15.0),
            Pos2::new(card.right() - 15.0, card.top() + card.height() * 0.55),
        );
        if sheen.is_positive() {
            painter.add(
                RectShape::filled(
                    sheen,
                    CornerRadius::same(14),
                    alpha(Color32::from_white_alpha(11)),
                )
                .with_blur_width(26.0)
                .with_round_to_pixels(false),
            );
        }
    }

    // Two rims: a neutral hairline that gives the glass an edge, and the accent tint on top of
    // it so the card's colour tells you which phase you are in without reading anything.
    painter.add(
        RectShape::stroke(
            card,
            radius,
            Stroke::new(1.0, alpha(palette.rim)),
            StrokeKind::Inside,
        )
        .with_round_to_pixels(false),
    );
    painter.add(
        RectShape::stroke(
            card,
            radius,
            Stroke::new(1.0, alpha(palette.border)),
            StrokeKind::Inside,
        )
        .with_round_to_pixels(false),
    );
}

/// What the card says in each phase. The body text keeps its slot across phases, so the live
/// transcript stays put while the header around it changes.
struct Content {
    title: &'static str,
    body: String,
    body_muted: bool,
    meter: Meter,
    /// Result text is sized to fit; live text keeps a stable width so it does not jitter as
    /// words arrive.
    measured: bool,
}

enum Meter {
    Wave,
    Sweep,
    Elapsed,
    None,
}

impl Content {
    fn for_snapshot(snapshot: &OsdSnapshot) -> Self {
        match snapshot.phase {
            OsdPhase::Listening => Self {
                title: "正在聆听",
                body: if snapshot.partial.is_empty() {
                    format!("松开 {} 插入文字", snapshot.hotkey)
                } else {
                    snapshot.partial.clone()
                },
                body_muted: snapshot.partial.is_empty(),
                meter: Meter::Wave,
                measured: false,
            },
            OsdPhase::Processing => Self {
                title: "正在整理",
                body: if snapshot.partial.is_empty() {
                    "转写中…".to_owned()
                } else {
                    snapshot.partial.clone()
                },
                body_muted: snapshot.partial.is_empty(),
                meter: Meter::Sweep,
                measured: false,
            },
            OsdPhase::Done => Self {
                title: "已插入",
                body: snapshot.text.clone(),
                body_muted: false,
                meter: Meter::Elapsed,
                measured: true,
            },
            OsdPhase::Notice => Self {
                title: "未插入",
                body: snapshot.text.clone(),
                body_muted: false,
                meter: Meter::None,
                measured: true,
            },
            OsdPhase::Hidden => Self {
                title: "",
                body: String::new(),
                body_muted: true,
                meter: Meter::None,
                measured: true,
            },
        }
    }

    fn body_color(&self, palette: &Palette) -> Color32 {
        if self.body_muted {
            palette.muted
        } else {
            palette.text
        }
    }

    fn target_width(&self, ui: &egui::Ui) -> f32 {
        if !self.measured {
            return CARD_WIDTH_LIVE;
        }
        let measured = ui
            .painter()
            .layout_no_wrap(
                self.body.clone(),
                FontId::proportional(BODY_FONT),
                Color32::WHITE,
            )
            .size()
            .x;
        // Text that has to wrap is spread evenly over its lines rather than filling the card to
        // the brim and leaving a couple of orphaned characters on the last one.
        let usable = CARD_WIDTH_MAX - BODY_LEFT - BODY_RIGHT;
        let rows = (measured / usable).ceil().max(1.0);
        (measured / rows + BODY_LEFT + BODY_RIGHT).clamp(CARD_WIDTH_MIN, CARD_WIDTH_MAX)
    }
}

/// Lay the transcript out, keeping the newest [`BODY_MAX_ROWS`] lines. Speech arrives faster
/// than a fixed-size card can grow, so the card scrolls rather than pushing the tail off-screen.
fn body_galley(
    ui: &egui::Ui,
    text: &str,
    max_width: f32,
    color: Color32,
) -> std::sync::Arc<egui::Galley> {
    let max_width = max_width.max(40.0);
    let mut start = 0usize;
    loop {
        let job = egui::text::LayoutJob::simple(
            text[start..].to_owned(),
            FontId::proportional(BODY_FONT),
            color,
            max_width,
        );
        let galley = ui.painter().layout_job(job);
        if galley.rows.len() <= BODY_MAX_ROWS {
            return galley;
        }
        // `Row::glyphs` holds one entry per char, so the first row tells us exactly how far to
        // scroll to drop it.
        let drop = galley.rows[0].glyphs.len().max(1);
        let Some((offset, _)) = text[start..].char_indices().nth(drop) else {
            return galley;
        };
        start += offset;
    }
}

/// The status orb: a breathing ring while listening, an orbiting arc while transcribing,
/// a tick or an exclamation once it is over.
fn draw_orb(painter: &egui::Painter, center: Pos2, snapshot: &OsdSnapshot, accent: Color32) {
    let glow = |radius: f32, strength: f32| {
        painter.add(
            RectShape::filled(
                Rect::from_center_size(center, Vec2::splat(radius * 2.0)),
                CornerRadius::same(radius.round().min(255.0) as u8),
                accent.gamma_multiply(strength),
            )
            .with_blur_width(radius * 0.9)
            .with_round_to_pixels(false),
        );
    };

    match snapshot.phase {
        OsdPhase::Listening => {
            glow(13.0 + snapshot.level * 5.0, 0.30);
            painter.circle_filled(center, 5.5 + snapshot.level * 2.5, accent);
        }
        OsdPhase::Processing => {
            glow(13.0, 0.24);
            draw_arc(
                painter,
                center,
                10.0,
                snapshot.changed_at.elapsed().as_secs_f32() * 4.2,
                accent,
            );
        }
        OsdPhase::Done => {
            glow(13.0, 0.26);
            let stroke = Stroke::new(2.2, accent);
            painter.line_segment(
                [center + Vec2::new(-5.4, 0.2), center + Vec2::new(-1.4, 4.2)],
                stroke,
            );
            painter.line_segment(
                [center + Vec2::new(-1.4, 4.2), center + Vec2::new(6.0, -4.4)],
                stroke,
            );
        }
        OsdPhase::Notice => {
            glow(13.0, 0.26);
            painter.line_segment(
                [center + Vec2::new(0.0, -5.6), center + Vec2::new(0.0, 1.6)],
                Stroke::new(2.2, accent),
            );
            painter.circle_filled(center + Vec2::new(0.0, 5.4), 1.4, accent);
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
    painter.add(Shape::line(points, Stroke::new(2.2, color)));
}

/// Right-hand side of the header: what the microphone hears, how long it has been running, or
/// how far along the transcription is.
fn draw_meter(
    painter: &egui::Painter,
    card: Rect,
    header_y: f32,
    snapshot: &OsdSnapshot,
    content: &Content,
    palette: &Palette,
    progress: f32,
) {
    let alpha = |color: Color32| color.gamma_multiply(progress);
    let right = card.right() - BODY_RIGHT;
    let timer = |text: String| {
        painter.text(
            Pos2::new(right, header_y),
            Align2::RIGHT_CENTER,
            text,
            FontId::proportional(12.0),
            alpha(palette.muted),
        );
    };

    match content.meter {
        Meter::Wave => {
            timer(format!("{:.1}s", snapshot.elapsed.as_secs_f32()));
            let wave_right = right - TIMER_WIDTH;
            draw_waveform(
                painter,
                Rect::from_min_max(
                    Pos2::new(wave_right - WAVE_WIDTH, header_y - 13.0),
                    Pos2::new(wave_right, header_y + 13.0),
                ),
                &snapshot.levels,
                alpha(palette.accent),
            );
        }
        Meter::Sweep => draw_sweep(
            painter,
            Rect::from_min_max(
                Pos2::new(right - 92.0, header_y - 2.5),
                Pos2::new(right, header_y + 2.5),
            ),
            snapshot.changed_at.elapsed(),
            alpha(palette.accent),
        ),
        Meter::Elapsed => timer(format!("{:.1}s", snapshot.elapsed.as_secs_f32())),
        Meter::None => {}
    }
}

/// Scrolling history of the microphone level: what was actually heard, not a canned animation.
///
/// `pub(crate)` so the isolated Wayland OSD process ([`crate::wayland_osd`]) can paint the same
/// waveform from the level history it receives over IPC, instead of maintaining a second copy.
pub(crate) fn draw_waveform(painter: &egui::Painter, rect: Rect, levels: &[f32], color: Color32) {
    if rect.width() <= 0.0 || levels.is_empty() {
        return;
    }
    let step = rect.width() / levels.len() as f32;
    let center_y = rect.center().y;
    for (index, level) in levels.iter().enumerate() {
        let x = rect.left() + step * (index as f32 + 0.5);
        // Newer samples on the right are drawn brighter, so the bar reads as moving.
        let recency = (index as f32 / levels.len() as f32).powf(1.6);
        let height = (2.5 + level.sqrt() * rect.height() * 0.92).min(rect.height());
        painter.add(
            RectShape::filled(
                Rect::from_center_size(Pos2::new(x, center_y), Vec2::new(2.4, height)),
                CornerRadius::same(1),
                color.gamma_multiply(0.22 + 0.78 * recency),
            )
            .with_round_to_pixels(false),
        );
    }
}

fn draw_sweep(painter: &egui::Painter, track: Rect, elapsed: Duration, color: Color32) {
    painter.add(
        RectShape::filled(track, CornerRadius::same(3), color.gamma_multiply(0.18))
            .with_round_to_pixels(false),
    );
    // Indeterminate: a shuttle sweeping the track, since ASR gives us no progress to report.
    let cycle = (elapsed.as_secs_f32() * 0.9).fract();
    let eased = 0.5 - 0.5 * (cycle * std::f32::consts::TAU).cos();
    let width = track.width() * 0.42;
    let left = track.left() + (track.width() - width) * eased;
    painter.add(
        RectShape::filled(
            Rect::from_min_size(
                Pos2::new(left, track.top()),
                Vec2::new(width, track.height()),
            ),
            CornerRadius::same(3),
            color,
        )
        .with_round_to_pixels(false),
    );
}

struct Palette {
    surface: Color32,
    border: Color32,
    rim: Color32,
    accent: Color32,
    title: Color32,
    text: Color32,
    muted: Color32,
}

impl Palette {
    fn for_snapshot(snapshot: &OsdSnapshot, compositing: OverlayCompositing) -> Self {
        let accent = match snapshot.phase {
            OsdPhase::Hidden => Color32::from_rgb(112, 126, 151),
            OsdPhase::Listening => Color32::from_rgb(255, 92, 116),
            OsdPhase::Processing => Color32::from_rgb(104, 156, 255),
            OsdPhase::Done => Color32::from_rgb(67, 211, 151),
            OsdPhase::Notice if snapshot.warn => Color32::from_rgb(255, 122, 122),
            OsdPhase::Notice => Color32::from_rgb(235, 176, 91),
        };
        Self {
            // Translucent enough to sit in the page rather than on top of it, opaque enough to
            // keep the transcript readable over anything.
            surface: if compositing.blends() {
                Color32::from_rgba_unmultiplied(17, 21, 30, 224)
            } else {
                Color32::from_rgb(19, 24, 34)
            },
            border: accent.gamma_multiply(0.34),
            rim: if compositing.blends() {
                Color32::from_white_alpha(26)
            } else {
                Color32::from_rgb(48, 56, 72)
            },
            accent,
            title: Color32::from_rgb(176, 186, 204),
            text: Color32::from_rgb(245, 247, 250),
            muted: Color32::from_rgb(139, 150, 170),
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

    #[test]
    fn partial_text_streams_while_listening_and_survives_processing() {
        let handle = OsdHandle::new();
        handle.set_recording();
        handle.set_partial("今天");
        assert_eq!(handle.snapshot().partial, "今天");
        handle.set_partial("今天天气");
        handle.set_processing();
        assert_eq!(handle.snapshot().partial, "今天天气");
        handle.set_partial("今天天气不错");
        assert_eq!(handle.snapshot().partial, "今天天气不错");
    }

    #[test]
    fn partial_text_is_dropped_once_the_result_is_in() {
        let handle = OsdHandle::new();
        handle.set_recording();
        handle.set_partial("今天天气");
        handle.set_done("今天天气不错。");
        let snapshot = handle.snapshot();
        assert!(snapshot.partial.is_empty());
        assert_eq!(snapshot.text, "今天天气不错。");
        // A late preview from the worker must not overwrite the finished result.
        handle.set_partial("今天天气不");
        assert!(handle.snapshot().partial.is_empty());
    }
}
