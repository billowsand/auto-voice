//! `auto-voice osd-demo`: paint the dictation overlay over a stand-in document, so the card can
//! be reviewed — corners, translucency, layout, streaming text — without a microphone, a model
//! or a real dictation.
//!
//! The overlay is a native always-on-top window whose look depends on how the compositor blends
//! it, which is exactly the part that cannot be judged from the source.

use std::time::Instant;

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2, ViewportCommand,
    ViewportId,
};

use crate::osd::{self, OsdHandle, OsdPhase};

const SAMPLE: &str =
    "这个悬浮窗的圆角在 Windows 上要足够平滑，半透明的玻璃质感也要能透出下面的内容。";
const BACKDROP: Vec2 = Vec2::new(1000.0, 620.0);
/// Where the card's top-left corner is pinned, in logical points on the backdrop.
const OVERLAY_AT: Pos2 = Pos2::new(250.0, 300.0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DemoPhase {
    /// Listening, nothing recognised yet: the card is at its smallest.
    Waiting,
    /// Listening with the transcript streaming in.
    Listening,
    /// Hotkey released, transcript on screen while the text is polished.
    Processing,
    Done,
    Notice,
}

impl std::str::FromStr for DemoPhase {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "waiting" => Ok(Self::Waiting),
            "listening" => Ok(Self::Listening),
            "processing" => Ok(Self::Processing),
            "done" => Ok(Self::Done),
            "notice" => Ok(Self::Notice),
            other => Err(format!(
                "unknown phase \"{other}\" (waiting|listening|processing|done|notice)"
            )),
        }
    }
}

pub fn run(phase: DemoPhase) -> anyhow::Result<()> {
    let osd = OsdHandle::new();
    osd.set_follow_caret(false);

    let viewport = egui::ViewportBuilder::default()
        .with_title("auto-voice overlay demo")
        .with_inner_size(BACKDROP)
        .with_position(Pos2::ZERO)
        .with_always_on_top()
        .with_resizable(false);

    eframe::run_native(
        "auto-voice-osd-demo",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            viewport,
            ..Default::default()
        },
        Box::new(move |creation| {
            crate::platform::install_system_fonts(&creation.egui_ctx);
            osd.attach_context(&creation.egui_ctx);
            std::thread::spawn({
                let osd = osd.clone();
                move || drive(osd, phase)
            });
            Ok(Box::new(DemoApp {
                osd,
                clicks: 0,
                started: Instant::now(),
                overlay_frames: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                last_click: None,
            }))
        }),
    )
    .map_err(|error| anyhow::anyhow!("overlay demo failed: {error}"))
}

struct DemoApp {
    osd: OsdHandle,
    /// Clicks the backdrop received. The overlay is click-through, so a click aimed at the card
    /// has to land here — that is the only way to tell from a screenshot.
    clicks: usize,
    started: Instant,
    overlay_frames: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    last_click: Option<Pos2>,
}

/// Drive the overlay from a worker thread, the way the dictation loop does: the audio pipeline
/// pushes levels and partial transcripts in from off the UI thread, and that is what the
/// overlay's repainting has to keep up with.
fn drive(osd: OsdHandle, phase: DemoPhase) {
    let mut since = Instant::now();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(16));
        if osd.snapshot().phase == OsdPhase::Hidden {
            osd.set_recording();
            since = Instant::now();
            continue;
        }

        let seconds = since.elapsed().as_secs_f32();
        let level =
            (0.5 + 0.5 * (seconds * 7.0).sin()) * (0.35 + 0.4 * (seconds * 1.7).cos().abs());

        match phase {
            DemoPhase::Waiting => osd.set_level(level),
            DemoPhase::Listening => {
                osd.set_level(level);
                osd.set_partial(streamed(seconds));
            }
            // Set once and then left alone: while the recogniser and the LLM are working, the
            // overlay has to keep its own sweep and spinner going with nothing pushing it.
            DemoPhase::Processing if osd.snapshot().phase != OsdPhase::Processing => {
                osd.set_partial(SAMPLE);
                osd.set_processing();
            }
            DemoPhase::Processing => {}
            // `set_done` / `set_notice` restart the linger timer, so re-issuing them holds the
            // result card on screen indefinitely for a good look at it.
            DemoPhase::Done => osd.set_done(SAMPLE),
            DemoPhase::Notice => osd.set_notice("没有听清，再按住试一次", false),
        }
    }
}

impl DemoApp {
    fn paint_click_probe(&self, ui: &egui::Ui) {
        let painter = ui.painter();
        painter.text(
            Pos2::new(ui.max_rect().left() + 48.0, ui.max_rect().bottom() - 30.0),
            egui::Align2::LEFT_BOTTOM,
            format!(
                "overlay: {} frames in {:.0}s  ·  clicks landing on the backdrop: {}",
                self.overlay_frames
                    .load(std::sync::atomic::Ordering::Relaxed),
                self.started.elapsed().as_secs_f32(),
                self.clicks
            ),
            FontId::proportional(14.0),
            Color32::from_rgb(90, 96, 110),
        );
        if let Some(at) = self.last_click {
            painter.circle_filled(at, 9.0, Color32::from_rgb(220, 40, 60));
        }
    }
}

/// One character every 90ms, looping, so a screenshot lands on a half-finished transcript.
fn streamed(seconds: f32) -> &'static str {
    let total = SAMPLE.chars().count();
    let shown = ((seconds / 0.09) as usize % (total + 12)).min(total);
    let end = SAMPLE
        .char_indices()
        .nth(shown)
        .map_or(SAMPLE.len(), |(offset, _)| offset);
    &SAMPLE[..end]
}

impl eframe::App for DemoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let monitor_size = ui.ctx().input(|input| input.viewport().monitor_size);
        let handle = self.osd.clone();
        let painted = self.overlay_frames.clone();
        ui.ctx().show_viewport_deferred(
            osd::viewport_id(),
            osd::viewport_builder(monitor_size, true),
            move |ui, _class| {
                painted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                osd::draw(ui, &handle);
            },
        );

        if ui.input(|input| input.pointer.any_pressed()) {
            self.clicks += 1;
            self.last_click = ui.input(|input| input.pointer.interact_pos());
        }
        paint_backdrop(ui);
        self.paint_click_probe(ui);
        // The backdrop is static; it only needs to redraw for the counters.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
        // Both windows are pinned, so a screenshot always frames the card the same way. Only
        // for the first few seconds though: the overlay window does not exist on frame one, and
        // re-sending a position every frame forever stalls winit's event loop.
        if self.started.elapsed() < std::time::Duration::from_secs(3) {
            ui.ctx()
                .send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::OuterPosition(Pos2::ZERO));
            ui.ctx().send_viewport_cmd_to(
                osd::viewport_id(),
                ViewportCommand::OuterPosition(OVERLAY_AT - osd::card_inset()),
            );
        }
    }

    /// The overlay viewport inherits this, and anything but a fully transparent clear paints the
    /// whole canvas instead of just the card.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

/// A light document on the left, a saturated image-like panel on the right. The card straddles
/// both, which is the only honest way to judge a translucent surface.
fn paint_backdrop(ui: &mut egui::Ui) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    let split = rect.left() + rect.width() * 0.52;

    painter.rect_filled(
        Rect::from_min_max(rect.min, Pos2::new(split, rect.bottom())),
        CornerRadius::ZERO,
        Color32::from_rgb(246, 246, 248),
    );
    for row in 0..24 {
        let y = rect.top() + 46.0 + row as f32 * 22.0;
        let width = 300.0 + ((row * 37) % 130) as f32;
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(rect.left() + 48.0, y), Vec2::new(width, 9.0)),
            CornerRadius::same(4),
            Color32::from_rgb(203, 206, 214),
        );
    }

    // A coarse gradient: enough colour and contrast behind the card to show any fringing.
    let steps = 48;
    for step in 0..steps {
        let t = step as f32 / (steps - 1) as f32;
        let band = Rect::from_min_max(
            Pos2::new(split, rect.top() + rect.height() * t),
            Pos2::new(
                rect.right(),
                rect.top() + rect.height() * (t + 1.05 / steps as f32),
            ),
        );
        painter.rect_filled(
            band,
            CornerRadius::ZERO,
            Color32::from_rgb(
                (26.0 + 200.0 * t) as u8,
                (58.0 + 60.0 * (1.0 - t)) as u8,
                (168.0 - 70.0 * t) as u8,
            ),
        );
    }

    painter.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, Color32::from_rgb(120, 120, 130)),
        StrokeKind::Inside,
    );
    painter.text(
        Pos2::new(rect.left() + 48.0, rect.top() + 16.0),
        egui::Align2::LEFT_TOP,
        "overlay demo backdrop",
        FontId::proportional(13.0),
        Color32::from_rgb(150, 154, 164),
    );
}
