//! eframe desktop shell: the first-run wizard, the settings window, and the OSD viewport.
//!
//! Two rules shape this file:
//! * A fresh install opens the wizard; a configured install never shows a window on launch,
//!   it just lands in the tray.
//! * Nothing here asks for a restart. Every edit is saved shortly after you stop typing and
//!   pushed into the running pipeline through [`Runtime`].

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, CornerRadius, Frame, Id, Margin, Rect, RichText, Sense, Stroke, StrokeKind,
    Vec2, ViewportCommand, ViewportId,
};
use tray_icon::menu::{MenuEvent, MenuId};
use tray_icon::TrayIconEvent;

use crate::runtime::{EngineStatus, Runtime};
use crate::{config, config::ConfigFile, osd, platform};

/// How long after the last edit the config file is written and pushed to the pipeline.
const AUTOSAVE_DELAY: Duration = Duration::from_millis(600);
const TOAST_VISIBLE_FOR: Duration = Duration::from_millis(2600);

// ── Palette ──────────────────────────────────────────────────────────────────

const BG: Color32 = Color32::from_rgb(13, 17, 24);
const RAIL: Color32 = Color32::from_rgb(16, 21, 30);
const CARD: Color32 = Color32::from_rgb(21, 27, 38);
const CARD_HOVER: Color32 = Color32::from_rgb(26, 33, 46);
const LINE: Color32 = Color32::from_rgb(38, 48, 65);
const TEXT: Color32 = Color32::from_rgb(237, 241, 248);
const MUTED: Color32 = Color32::from_rgb(133, 147, 169);
const ACCENT: Color32 = Color32::from_rgb(88, 132, 240);
const OK: Color32 = Color32::from_rgb(76, 211, 155);
const WARN: Color32 = Color32::from_rgb(235, 176, 91);
const BAD: Color32 = Color32::from_rgb(255, 104, 125);

pub struct TrayActions {
    pub open_settings: MenuId,
    pub quit: MenuId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Overview,
    Input,
    Model,
    Polish,
    Appearance,
}

impl Section {
    const ALL: [(Section, &'static str, &'static str); 5] = [
        (Section::Overview, "总览", "运行状态与用法"),
        (Section::Input, "说话方式", "快捷键与浮层"),
        (Section::Model, "识别模型", "后端、语言与路径"),
        (Section::Polish, "文本优化", "LLM 纠错"),
        (Section::Appearance, "外观", "界面字体"),
    ];
}

struct Toast {
    message: String,
    error: bool,
    at: Instant,
}

pub struct DesktopApp {
    osd: osd::OsdHandle,
    runtime: Runtime,
    settings: ConfigFile,
    capabilities: platform::Capabilities,
    exit_requested: Arc<AtomicBool>,
    /// `Some` while the wizard is running, holding the current step.
    wizard_step: Option<usize>,
    section: Section,
    dirty_since: Option<Instant>,
    /// The window is centred on the first frame; winit's default placement can hang off-screen.
    centred: bool,
    toast: Option<Toast>,
    input_devices: Vec<String>,
    default_input_device: Option<String>,
    ui_context: egui::Context,
    system_fonts: Vec<platform::SystemFont>,
    font_search: String,
    applied_font_families: Option<Vec<String>>,
    /// Wayland ignores `with_visible(false)` at window creation, so a configured install has to
    /// hide itself during the first frames instead of starting hidden. Counts down the retries;
    /// 0 means done (or nothing to do).
    #[cfg(target_os = "linux")]
    hide_on_start_attempts: u8,
    _tray: tray_icon::TrayIcon,
}

impl DesktopApp {
    pub fn new(
        creation: &eframe::CreationContext<'_>,
        osd: osd::OsdHandle,
        runtime: Runtime,
        settings: ConfigFile,
        tray: tray_icon::TrayIcon,
        actions: TrayActions,
    ) -> Self {
        let system_fonts = platform::list_system_fonts();
        platform::install_ui_fonts(
            &creation.egui_ctx,
            settings.ui_font_families.as_deref(),
            &system_fonts,
        );
        install_theme(&creation.egui_ctx);
        osd.attach_context(&creation.egui_ctx);

        let exit_requested = Arc::new(AtomicBool::new(false));
        let exit_for_handler = Arc::clone(&exit_requested);
        let context = creation.egui_ctx.clone();

        // A hidden Wayland window can leave winit waiting indefinitely even though background
        // PTT and tray threads are still active. Keep a lightweight wake source outside the UI
        // loop so compositor pings, Ctrl+C shutdown, and tray actions are always dispatched.
        #[cfg(target_os = "linux")]
        {
            let context = context.clone();
            let exit_requested = Arc::clone(&exit_requested);
            std::thread::spawn(move || {
                while !exit_requested.load(Ordering::SeqCst) {
                    context.request_repaint();
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
        }

        // Clicking the tray icon opens settings, the way every other tray app behaves.
        let context_for_tray = context.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            let opens = match event {
                TrayIconEvent::Click {
                    button,
                    button_state,
                    ..
                } => {
                    button == tray_icon::MouseButton::Left
                        && button_state == tray_icon::MouseButtonState::Up
                }
                TrayIconEvent::DoubleClick { button, .. } => button == tray_icon::MouseButton::Left,
                _ => false,
            };
            if opens {
                show_settings_window(&context_for_tray);
            }
        }));

        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == actions.open_settings {
                show_settings_window(&context);
            } else if event.id == actions.quit {
                exit_for_handler.store(true, Ordering::SeqCst);
                context.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
                context.request_repaint();
            }
        }));

        let wizard_step = (!settings.is_configured()).then_some(0);
        let applied_font_families = settings.ui_font_families.clone();
        let input_devices = crate::audio::input_device_names().unwrap_or_default();
        let default_input_device = crate::audio::default_input_device_name();

        Self {
            osd,
            runtime,
            settings,
            capabilities: platform::capabilities(),
            exit_requested,
            wizard_step,
            section: Section::Overview,
            dirty_since: None,
            centred: false,
            toast: None,
            input_devices,
            default_input_device,
            ui_context: creation.egui_ctx.clone(),
            system_fonts,
            font_search: String::new(),
            applied_font_families,
            #[cfg(target_os = "linux")]
            hide_on_start_attempts: if wizard_step.is_none() { 100 } else { 0 },
            _tray: tray,
        }
    }

    // ── Saving ───────────────────────────────────────────────────────────────

    /// Called by every editable widget. The actual write is debounced so typing a URL does not
    /// produce one file write per keystroke.
    fn touched(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    fn commit(&mut self) {
        self.dirty_since = None;
        match self.settings.save() {
            Ok(_) => {
                let reloading = self.runtime.apply(&self.settings);
                let font_report = (self.applied_font_families != self.settings.ui_font_families)
                    .then(|| {
                        self.applied_font_families = self.settings.ui_font_families.clone();
                        platform::install_ui_fonts(
                            &self.ui_context,
                            self.settings.ui_font_families.as_deref(),
                            &self.system_fonts,
                        )
                    });
                let live = self.runtime.live();
                self.osd
                    .set_hotkey_label(config::describe_ptt_key(&live.ptt_key));
                self.osd.set_follow_caret(live.follow_caret);
                let font_error = font_report
                    .as_ref()
                    .is_some_and(|report| !report.errors.is_empty());
                self.toast = Some(Toast {
                    message: if font_error {
                        let font_report = font_report.as_ref().expect("font report was checked");
                        format!(
                            "已保存 · 已加载 {} 个字体，{} 个字体不可用",
                            font_report.loaded,
                            font_report.errors.len()
                        )
                    } else if reloading {
                        "已保存 · 正在重新加载模型".to_owned()
                    } else {
                        "已保存 · 立即生效".to_owned()
                    },
                    error: font_error,
                    at: Instant::now(),
                });
            }
            Err(error) => {
                self.toast = Some(Toast {
                    message: format!("保存失败：{error:#}"),
                    error: true,
                    at: Instant::now(),
                });
            }
        }
    }

    fn flush_pending_save(&mut self) {
        if self.dirty_since.is_some() {
            self.commit();
        }
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.flush_pending_save();
        #[cfg(target_os = "linux")]
        {
            platform::hide_main_window(ctx);
        }
        #[cfg(not(target_os = "linux"))]
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(false));
    }

    // ── Wizard ───────────────────────────────────────────────────────────────

    fn wizard_ui(&mut self, ui: &mut egui::Ui, step: usize) {
        page(ui, |ui| {
            ui.add_space(6.0);
            step_dots(ui, step, 3);
            ui.add_space(20.0);

            match step {
                0 => self.wizard_welcome(ui),
                1 => self.wizard_hotkey(ui),
                _ => self.wizard_finish(ui),
            }

            ui.add_space(22.0);
            ui.horizontal(|ui| {
                if step > 0 && ghost_button(ui, "上一步").clicked() {
                    self.wizard_step = Some(step - 1);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let last = step == 2;
                    let label = if last {
                        "完成，开始使用"
                    } else {
                        "下一步"
                    };
                    if primary_button(ui, label).clicked() {
                        if last {
                            self.finish_wizard(ui.ctx());
                        } else {
                            self.wizard_step = Some(step + 1);
                        }
                    }
                    if !last && ghost_button(ui, "跳过设置").clicked() {
                        self.finish_wizard(ui.ctx());
                    }
                });
            });
        });
    }

    fn wizard_welcome(&mut self, ui: &mut egui::Ui) {
        heading(ui, "欢迎使用 Auto Voice", "在任何输入框里，按住一个键说话");
        ui.add_space(16.0);
        card(ui, None, |ui| {
            bullet(
                ui,
                "1",
                "按住快捷键",
                "浮层会在光标附近弹出，实时显示你的声音",
            );
            bullet(ui, "2", "松开快捷键", "本地模型转写，不联网、不上传");
            bullet(
                ui,
                "3",
                "文字自动落到光标处",
                "识别结果会直接粘贴进当前输入框",
            );
        });
        ui.add_space(12.0);
        let input_device = self.effective_input_device_name();
        capability_card(ui, &self.capabilities, input_device.as_deref());
    }

    fn wizard_hotkey(&mut self, ui: &mut egui::Ui) {
        heading(ui, "选一个按住说话的键", "按住时录音，松开就插入文字");
        ui.add_space(16.0);
        if self.hotkey_picker(ui) {
            self.touched();
        }
        ui.add_space(12.0);
        hint(
            ui,
            "选 Caps Lock 时，程序只在按住期间录音，短按仍然可以正常切换大小写。",
        );
    }

    fn wizard_finish(&mut self, ui: &mut egui::Ui) {
        heading(ui, "最后两件事", "都可以随时在设置里改");
        ui.add_space(16.0);
        card(
            ui,
            Some(("识别模型", "本地运行，第一次加载需要几秒")),
            |ui| {
                model_status_row(ui, &self.runtime, &self.settings);
            },
        );
        ui.add_space(12.0);
        let mut changed = false;
        card(
            ui,
            Some(("文本优化", "可选：用本地 LM Studio 顺一遍语句")),
            |ui| {
                let mut polish = !self.settings.no_llm.unwrap_or(false);
                if toggle_row(
                    ui,
                    &mut polish,
                    "启用 LLM 纠错",
                    "关闭后直接插入原始识别结果，速度更快",
                ) {
                    self.settings.no_llm = Some(!polish);
                    changed = true;
                }
            },
        );
        if changed {
            self.touched();
        }
        ui.add_space(12.0);
        hint(
            ui,
            "完成后窗口会收进系统托盘，随时可以从托盘图标重新打开设置。",
        );
    }

    fn finish_wizard(&mut self, ctx: &egui::Context) {
        self.settings.setup_done = Some(true);
        if self.settings.ptt_key.is_none() {
            self.settings.ptt_key = Some("CapsLock".to_owned());
        }
        self.wizard_step = None;
        self.commit();
        #[cfg(target_os = "linux")]
        {
            platform::hide_main_window(ctx);
        }
        #[cfg(not(target_os = "linux"))]
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(false));
        let live = self.runtime.live();
        self.osd.set_notice(
            format!(
                "已在后台运行 · 按住 {} 说话",
                config::describe_ptt_key(&live.ptt_key)
            ),
            false,
        );
    }

    // ── Settings ─────────────────────────────────────────────────────────────

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("auto-voice-nav")
            .exact_size(188.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(RAIL).inner_margin(Margin::same(14)))
            .show(ui, |ui| self.nav_ui(ui));

        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(24, 20)),
            )
            .show(ui, |ui| {
                self.status_header(ui);
                ui.add_space(16.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.section {
                        Section::Overview => self.overview_section(ui),
                        Section::Input => self.input_section(ui),
                        Section::Model => self.model_section(ui),
                        Section::Polish => self.polish_section(ui),
                        Section::Appearance => self.appearance_section(ui),
                    });
            });
    }

    fn nav_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(
            RichText::new("AUTO VOICE")
                .size(11.0)
                .strong()
                .color(ACCENT),
        );
        ui.label(RichText::new("语音输入").size(19.0).strong().color(TEXT));
        ui.add_space(18.0);

        for (section, label, description) in Section::ALL {
            if nav_item(ui, self.section == section, label, description).clicked() {
                self.section = section;
            }
            ui.add_space(4.0);
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(4.0);
            ui.label(
                RichText::new(self.settings.display_path().display().to_string())
                    .size(9.5)
                    .color(Color32::from_rgb(88, 99, 118)),
            );
            ui.label(RichText::new("配置文件").size(10.0).color(MUTED));
            ui.add_space(10.0);
            if ghost_button(ui, "收进托盘").clicked() {
                let ctx = ui.ctx().clone();
                self.hide_window(&ctx);
            }
        });
    }

    fn status_header(&mut self, ui: &mut egui::Ui) {
        let live = self.runtime.live();
        let (color, label) = match self.runtime.status() {
            EngineStatus::Ready => (OK, "识别引擎就绪".to_owned()),
            EngineStatus::Loading => (WARN, "正在加载识别模型…".to_owned()),
            EngineStatus::Failed(_) => (BAD, "识别引擎未就绪".to_owned()),
        };
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, 9.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.5, color);
            ui.label(RichText::new(label).size(13.0).color(TEXT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                chip(
                    ui,
                    &format!("按住 {}", config::describe_ptt_key(&live.ptt_key)),
                );
                chip(
                    ui,
                    &format!(
                        "{} · {}",
                        self.capabilities.platform_name, self.capabilities.session_name
                    ),
                );
            });
        });
        if let EngineStatus::Failed(error) = self.runtime.status() {
            ui.add_space(6.0);
            ui.label(RichText::new(error).size(11.5).color(BAD));
        }
    }

    fn overview_section(&mut self, ui: &mut egui::Ui) {
        let live = self.runtime.live();
        card(
            ui,
            Some(("怎么用", "任何可以打字的地方都能用")),
            |ui| {
                bullet(
                    ui,
                    "1",
                    &format!("按住 {}", config::describe_ptt_key(&live.ptt_key)),
                    "浮层弹出，开始录音",
                );
                bullet(ui, "2", "说话", "浮层里的波形跟着你的声音走");
                bullet(ui, "3", "松开", "转写完成后文字自动插入光标处");
            },
        );
        ui.add_space(12.0);
        let input_device = self.effective_input_device_name();
        capability_card(ui, &self.capabilities, input_device.as_deref());
        ui.add_space(12.0);
        card(
            ui,
            Some(("识别模型", "切换模型不需要重启程序")),
            |ui| {
                model_status_row(ui, &self.runtime, &self.settings);
            },
        );
    }

    fn input_section(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut refresh_devices = false;
        card(
            ui,
            Some(("麦克风", "选择按住说话时使用的输入设备")),
            |ui| {
                let previous = self.settings.input_device.clone();
                let mut selected = previous.clone();
                let default_label = self
                    .default_input_device
                    .as_deref()
                    .map(|name| format!("系统默认 · {name}"))
                    .unwrap_or_else(|| "系统默认".to_owned());
                let selected_label = selected.clone().unwrap_or_else(|| default_label.clone());

                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("input-device-picker")
                        .width((ui.available_width() - 96.0).max(220.0))
                        .selected_text(selected_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut selected, None, &default_label);
                            for name in &self.input_devices {
                                let label = if self.default_input_device.as_deref() == Some(name) {
                                    format!("{name} · 系统默认")
                                } else {
                                    name.clone()
                                };
                                ui.selectable_value(&mut selected, Some(name.clone()), label);
                            }
                            if let Some(missing) = previous.as_ref().filter(|name| {
                                !self.input_devices.iter().any(|device| device == *name)
                            }) {
                                ui.selectable_value(
                                    &mut selected,
                                    Some(missing.clone()),
                                    format!("{missing} · 当前不可用"),
                                );
                            }
                        });
                    if ui.small_button("重新扫描").clicked() {
                        refresh_devices = true;
                    }
                });

                if let Some(name) = selected
                    .as_ref()
                    .filter(|name| !self.input_devices.iter().any(|device| device == *name))
                {
                    status_line(
                        ui,
                        WARN,
                        &format!("{name} 当前不可用，录音时会自动回退到系统默认麦克风"),
                    );
                } else {
                    hint(
                        ui,
                        "切换将在下一次按住快捷键时生效，不会打断正在进行的录音。",
                    );
                }

                if selected != previous {
                    self.settings.input_device = selected;
                    changed = true;
                }
            },
        );
        if refresh_devices {
            self.refresh_input_devices();
        }
        ui.add_space(12.0);
        card(
            ui,
            Some(("按住说话快捷键", "按住时录音，松开后插入文字")),
            |ui| {
                changed |= self.hotkey_picker(ui);
            },
        );
        ui.add_space(12.0);
        card(ui, Some(("浮层", "录音提示窗口的行为")), |ui| {
            let mut follow = self.settings.overlay_follow_caret.unwrap_or(true);
            if toggle_row(
                ui,
                &mut follow,
                "跟随光标弹出",
                "关闭后固定显示在屏幕底部中央",
            ) {
                self.settings.overlay_follow_caret = Some(follow);
                changed = true;
            }
            ui.add_space(2.0);
            let mut preview = self.settings.overlay_live_preview.unwrap_or(true);
            if toggle_row(
                ui,
                &mut preview,
                "边说边显示转写",
                "说话时就把已识别的文字显示在浮层上，松开后再交给 AI 整理",
            ) {
                self.settings.overlay_live_preview = Some(preview);
                changed = true;
            }
            ui.add_space(6.0);
            hint(
                ui,
                "浮层只在按住快捷键时出现，识别结果展示一两秒后自动消失，不会常驻屏幕。",
            );
        });
        ui.add_space(12.0);
        card(
            ui,
            Some(("语音检测", "灵敏度只影响波形显示与静音判断")),
            |ui| {
                if setting_row(ui, "语音能量阈值", |ui| {
                    let value = self.settings.energy_threshold.get_or_insert(0.018);
                    ui.add(egui::Slider::new(value, 0.001..=0.12).logarithmic(true))
                        .changed()
                }) {
                    changed = true;
                }
                if setting_row(ui, "静音提交延迟", |ui| {
                    let value = self.settings.vad_silence_ms.get_or_insert(900);
                    ui.add(egui::Slider::new(value, 200..=3000).suffix(" ms"))
                        .changed()
                }) {
                    changed = true;
                }
            },
        );
        if changed {
            self.touched();
        }
    }

    fn refresh_input_devices(&mut self) {
        self.input_devices = crate::audio::input_device_names().unwrap_or_default();
        self.default_input_device = crate::audio::default_input_device_name();
    }

    fn effective_input_device_name(&self) -> Option<String> {
        self.settings
            .input_device
            .as_ref()
            .filter(|selected| self.input_devices.iter().any(|device| device == *selected))
            .cloned()
            .or_else(|| self.default_input_device.clone())
    }

    fn hotkey_picker(&mut self, ui: &mut egui::Ui) -> bool {
        let current = self
            .settings
            .ptt_key
            .clone()
            .unwrap_or_else(|| "CapsLock".to_owned());
        let mut changed = false;
        for &(spec, label, description) in config::PTT_PRESETS {
            if choice_row(ui, current == spec, label, description).clicked() {
                self.settings.ptt_key = Some(spec.to_owned());
                changed = true;
            }
            ui.add_space(6.0);
        }
        if !config::PTT_PRESETS
            .iter()
            .any(|(spec, ..)| *spec == current)
        {
            hint(ui, &format!("当前使用自定义组合键：{current}"));
        }
        changed
    }

    fn model_section(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        card(
            ui,
            Some(("识别后端", "切换后会在后台重新加载，无需重启")),
            |ui| {
                model_status_row(ui, &self.runtime, &self.settings);
                ui.add_space(10.0);
                let backend = self
                    .settings
                    .asr_backend
                    .clone()
                    .unwrap_or_else(|| "sense-voice".to_owned());
                for (value, label, description) in [
                    ("sense-voice", "SenseVoice", "中英日韩粤，速度快，默认"),
                    ("funasr-nano", "FunASR Nano", "中文更稳，占用更高"),
                ] {
                    if choice_row(ui, backend == value, label, description).clicked() {
                        self.settings.asr_backend = Some(value.to_owned());
                        changed = true;
                    }
                    ui.add_space(6.0);
                }
            },
        );
        ui.add_space(12.0);
        card(
            ui,
            Some(("语言", "指定语言通常比自动检测更准")),
            |ui| {
                let language = self
                    .settings
                    .lang
                    .clone()
                    .unwrap_or_else(|| "auto".to_owned());
                ui.horizontal_wrapped(|ui| {
                    for (value, label) in [
                        ("auto", "自动"),
                        ("zh", "中文"),
                        ("en", "English"),
                        ("ja", "日本語"),
                        ("ko", "한국어"),
                        ("yue", "粤语"),
                    ] {
                        if pill_button(ui, language == value, label).clicked() {
                            self.settings.lang = Some(value.to_owned());
                            changed = true;
                        }
                    }
                });
            },
        );
        ui.add_space(12.0);
        card(
            ui,
            Some(("模型路径", "相对路径以程序所在目录为准")),
            |ui| {
                if setting_row(ui, "SenseVoice 模型", |ui| {
                    let model = self
                        .settings
                        .model
                        .get_or_insert_with(|| crate::DEFAULT_MODEL.to_owned());
                    ui.add_sized([320.0, 30.0], egui::TextEdit::singleline(model))
                        .changed()
                }) {
                    changed = true;
                }
                if setting_row(ui, "SenseVoice 词表", |ui| {
                    let tokens = self
                        .settings
                        .tokens
                        .get_or_insert_with(|| crate::DEFAULT_TOKENS.to_owned());
                    ui.add_sized([320.0, 30.0], egui::TextEdit::singleline(tokens))
                        .changed()
                }) {
                    changed = true;
                }
            },
        );
        if changed {
            self.touched();
        }
    }

    fn polish_section(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut polish = !self.settings.no_llm.unwrap_or(false);
        card(
            ui,
            Some(("LLM 纠错", "用本地大模型顺一遍语句和标点")),
            |ui| {
                if toggle_row(
                    ui,
                    &mut polish,
                    "启用 LLM 纠错",
                    "关闭后直接插入原始识别结果",
                ) {
                    self.settings.no_llm = Some(!polish);
                    changed = true;
                }
            },
        );
        if polish {
            ui.add_space(12.0);
            card(ui, Some(("LM Studio", "本机 OpenAI 兼容接口")), |ui| {
                if setting_row(ui, "服务地址", |ui| {
                    let url = self
                        .settings
                        .lm_url
                        .get_or_insert_with(|| "http://localhost:1234".to_owned());
                    ui.add_sized([320.0, 30.0], egui::TextEdit::singleline(url))
                        .changed()
                }) {
                    changed = true;
                }
                if setting_row(ui, "模型名称", |ui| {
                    let model = self
                        .settings
                        .lm_model
                        .get_or_insert_with(|| "local-model".to_owned());
                    ui.add_sized([320.0, 30.0], egui::TextEdit::singleline(model))
                        .changed()
                }) {
                    changed = true;
                }
                ui.add_space(4.0);
                let mut auto_start = self.settings.lmstudio_auto_start.unwrap_or(false);
                if toggle_row(
                    ui,
                    &mut auto_start,
                    "随 Auto Voice 启动",
                    "启动时自动拉起 LM Studio 服务并加载模型",
                ) {
                    self.settings.lmstudio_auto_start = Some(auto_start);
                    changed = true;
                }
            });
            ui.add_space(12.0);
            hint(
                ui,
                "LM Studio 没开也不影响使用：调用失败时会直接插入原始识别结果。",
            );
        }
        if changed {
            self.touched();
        }
    }

    fn appearance_section(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut remove = None;
        let mut move_font = None;
        let mut restore_defaults = false;
        let mut add_family = None;

        card(
            ui,
            Some(("已选字体", "靠前的字体优先，缺少的字形由后续字体补全")),
            |ui| {
                let families = self.settings.ui_font_families.get_or_insert_with(Vec::new);
                if families.is_empty() {
                    hint(
                        ui,
                        "当前使用系统默认字体。从下方系统字体列表中点击即可添加。",
                    );
                } else {
                    for (index, family) in families.iter().enumerate() {
                        let installed = self
                            .system_fonts
                            .iter()
                            .any(|font| font.family.eq_ignore_ascii_case(family));
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{}", index + 1))
                                    .size(11.5)
                                    .color(MUTED),
                            );
                            ui.label(
                                RichText::new(if installed {
                                    family.clone()
                                } else {
                                    format!("{family}（未安装）")
                                })
                                .size(13.0)
                                .color(if installed {
                                    TEXT
                                } else {
                                    WARN
                                }),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("移除").clicked() {
                                        remove = Some(index);
                                    }
                                    if index + 1 < families.len() && ui.small_button("↓").clicked()
                                    {
                                        move_font = Some((index, index + 1));
                                    }
                                    if index > 0 && ui.small_button("↑").clicked() {
                                        move_font = Some((index, index - 1));
                                    }
                                },
                            );
                        });
                        if index + 1 < families.len() {
                            ui.separator();
                        }
                    }
                }

                if !families.is_empty() {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ghost_button(ui, "恢复默认").clicked() {
                            restore_defaults = true;
                        }
                    });
                }
            },
        );

        ui.add_space(12.0);
        let selected = self.settings.ui_font_families.clone().unwrap_or_default();
        card(
            ui,
            Some(("系统字体", "搜索并点击字体名称即可添加")),
            |ui| {
                ui.add_sized(
                    [ui.available_width(), 32.0],
                    egui::TextEdit::singleline(&mut self.font_search).hint_text("搜索系统字体…"),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("已检测到 {} 种字体", self.system_fonts.len()))
                        .size(10.5)
                        .color(MUTED),
                );
                ui.add_space(4.0);

                let query = self.font_search.trim().to_lowercase();
                let matches = self
                    .system_fonts
                    .iter()
                    .filter(|font| query.is_empty() || font.family.to_lowercase().contains(&query))
                    .collect::<Vec<_>>();
                if matches.is_empty() {
                    hint(ui, "没有找到匹配的系统字体。");
                    return;
                }

                egui::ScrollArea::vertical()
                    .id_salt("system-font-list")
                    .max_height(250.0)
                    .auto_shrink([false, true])
                    .show_rows(ui, 34.0, matches.len(), |ui, range| {
                        for font in &matches[range] {
                            let already_selected = selected
                                .iter()
                                .any(|family| family.eq_ignore_ascii_case(&font.family));
                            let label = if already_selected {
                                format!("✓  {}", font.family)
                            } else {
                                font.family.clone()
                            };
                            if ui
                                .add_enabled(
                                    !already_selected,
                                    egui::Button::new(
                                        RichText::new(label)
                                            .size(12.5)
                                            .color(if already_selected { OK } else { TEXT }),
                                    )
                                    .frame(false)
                                    .min_size(Vec2::new(ui.available_width(), 30.0)),
                                )
                                .clicked()
                            {
                                add_family = Some(font.family.clone());
                            }
                        }
                    });
            },
        );

        if let Some(index) = remove {
            if let Some(families) = self.settings.ui_font_families.as_mut() {
                families.remove(index);
            }
            changed = true;
        }
        if let Some((from, to)) = move_font {
            if let Some(families) = self.settings.ui_font_families.as_mut() {
                families.swap(from, to);
            }
            changed = true;
        }
        if let Some(family) = add_family {
            self.settings
                .ui_font_families
                .get_or_insert_with(Vec::new)
                .push(family);
            changed = true;
        }
        if restore_defaults {
            self.settings.ui_font_families = None;
            changed = true;
        }
        if self
            .settings
            .ui_font_families
            .as_ref()
            .is_some_and(Vec::is_empty)
        {
            self.settings.ui_font_families = None;
        }

        ui.add_space(12.0);
        hint(
            ui,
            "选择后会自动保存并立即替换设置页与浮层字体，无需重启程序。",
        );
        if changed {
            self.touched();
        }
    }

    fn toast_ui(&mut self, ui: &mut egui::Ui) {
        let Some(toast) = &self.toast else { return };
        let age = toast.at.elapsed();
        if age >= TOAST_VISIBLE_FOR {
            self.toast = None;
            return;
        }
        let fade = ((TOAST_VISIBLE_FOR - age).as_secs_f32() / 0.4).min(1.0);
        let color = if toast.error { BAD } else { OK };
        let anchor = ui.max_rect();
        egui::Area::new(Id::new("auto-voice-toast"))
            .fixed_pos(anchor.left_bottom() + Vec2::new(24.0, -56.0))
            .interactable(false)
            .show(ui.ctx(), |ui| {
                Frame::new()
                    .fill(CARD.gamma_multiply(fade))
                    .stroke(Stroke::new(1.0, color.gamma_multiply(0.55 * fade)))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(14, 9))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(&toast.message)
                                .size(12.5)
                                .color(TEXT.gamma_multiply(fade)),
                        );
                    });
            });
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }
}

impl eframe::App for DesktopApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "linux")]
        {
            // tray-icon's AppIndicator backend is GTK based, while eframe owns the native
            // event loop. Pump pending GTK work on the main thread so StatusNotifier items
            // are registered and menu clicks reach Omarchy/other Linux panels.
            let context = gtk::glib::MainContext::default();
            if context.pending() {
                context.iteration(false);
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested || self.runtime.shutdown_requested() {
            self.flush_pending_save();
            self.runtime.request_shutdown();
            self.exit_requested.store(true, Ordering::SeqCst);
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "linux")]
        if self.hide_on_start_attempts > 0 {
            self.hide_on_start_attempts -= 1;
            if platform::hide_main_window(ui.ctx()) {
                self.hide_on_start_attempts = 0;
            }
        }

        let monitor_size = ui.ctx().input(|input| input.viewport().monitor_size);
        let visible = self.osd.native_surface_visible();
        // Windows keeps the click-through surface alive to avoid focus stealing when it is
        // shown again. Other compositors can map an allegedly hidden transparent viewport as
        // an opaque/ghost rectangle, so do not create it until the OSD is actually needed.
        if cfg!(target_os = "windows") || visible {
            let osd_handle = self.osd.clone();
            ui.ctx().show_viewport_deferred(
                osd::viewport_id(),
                osd::viewport_builder(monitor_size, visible),
                move |ui, _class| osd::draw(ui, &osd_handle),
            );
        }

        if !self.centred {
            self.centred = true;
            let centre = ui.ctx().input(|input| {
                let viewport = input.viewport();
                let monitor = viewport.monitor_size?;
                let outer = viewport.outer_rect?;
                Some(egui::Pos2::new(
                    ((monitor.x - outer.width()) * 0.5).max(0.0),
                    ((monitor.y - outer.height()) * 0.5).max(0.0),
                ))
            });
            if let Some(position) = centre {
                ui.ctx().send_viewport_cmd_to(
                    ViewportId::ROOT,
                    ViewportCommand::OuterPosition(position),
                );
            }
        }

        match self.dirty_since {
            Some(since) if since.elapsed() >= AUTOSAVE_DELAY => self.commit(),
            Some(since) => ui
                .ctx()
                .request_repaint_after(AUTOSAVE_DELAY - since.elapsed()),
            None => {}
        }

        // The window itself is transparent (see `native_options`), so paint every pixel of it.
        ui.painter()
            .rect_filled(ui.max_rect(), CornerRadius::ZERO, BG);
        match self.wizard_step {
            Some(step) => self.wizard_ui(ui, step),
            None => self.settings_ui(ui),
        }
        self.toast_ui(ui);

        // The engine status lives on another thread; keep the header honest while it loads.
        if !self.runtime.status().is_ready() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flush_pending_save();
        self.runtime.request_shutdown();
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
        TrayIconEvent::set_event_handler::<fn(TrayIconEvent)>(None);
    }
}

fn show_settings_window(context: &egui::Context) {
    #[cfg(target_os = "linux")]
    platform::show_main_window(context);
    #[cfg(not(target_os = "linux"))]
    {
        context.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(true));
        context.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
    }
    context.request_repaint();
}

pub fn native_options(show_window: bool) -> eframe::NativeOptions {
    let icon = egui::IconData {
        rgba: include_bytes!("../assets/icons/auto-voice-32.rgba").to_vec(),
        width: 32,
        height: 32,
    };
    let viewport = egui::ViewportBuilder::default()
        .with_title("Auto Voice")
        .with_app_id("io.github.billowsand.auto-voice")
        .with_inner_size(Vec2::new(860.0, 620.0))
        .with_min_inner_size(Vec2::new(760.0, 560.0))
        .with_icon(icon)
        .with_visible(show_window);

    // The GL config is chosen once, from the root viewport, and the overlay needs an alpha
    // channel to composite. Windows is the exception: `transparent` maps to WGL's legacy
    // `TRANSPARENT_ARB` pixel formats, which no driver advertises, so asking for it can leave
    // glutin with no usable config at all. The alpha channel is there either way — glutin asks
    // for 8 bits by default — and `platform::prepare_overlay_window` gets DWM to blend it.
    #[cfg(not(target_os = "windows"))]
    let viewport = viewport.with_transparent(true);

    eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport,
        ..Default::default()
    }
}

fn install_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 9.0);
    style.spacing.button_padding = Vec2::new(14.0, 8.0);
    style.spacing.slider_width = 190.0;
    style.visuals.dark_mode = true;
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = CARD;
    style.visuals.extreme_bg_color = Color32::from_rgb(15, 20, 28);
    style.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.45);
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(8);
    ctx.set_style_of(egui::Theme::Dark, style);
}

// ── Reusable pieces ──────────────────────────────────────────────────────────

/// Comfortably narrow, horizontally centred column used by the wizard.
fn page(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    let width = (ui.available_width() - 80.0).clamp(280.0, 620.0);
    let left = (ui.available_width() - width) * 0.5;
    Frame::new()
        .inner_margin(Margin::symmetric(0, 26))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(left);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    add_contents(ui);
                });
            });
        });
}

fn heading(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(26.0).strong().color(TEXT));
    ui.add_space(4.0);
    ui.label(RichText::new(subtitle).size(13.0).color(MUTED));
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(MUTED));
}

fn chip(ui: &mut egui::Ui, text: &str) {
    Frame::new()
        .fill(CARD)
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.5).color(MUTED));
        });
}

fn card(ui: &mut egui::Ui, title: Option<(&str, &str)>, add_contents: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some((title, subtitle)) = title {
                ui.label(RichText::new(title).size(15.0).strong().color(TEXT));
                ui.label(RichText::new(subtitle).size(11.5).color(MUTED));
                ui.add_space(10.0);
            }
            add_contents(ui);
        });
}

fn setting_row(
    ui: &mut egui::Ui,
    label: &str,
    add_control: impl FnOnce(&mut egui::Ui) -> bool,
) -> bool {
    ui.horizontal(|ui| {
        ui.set_min_height(34.0);
        ui.add_sized(
            [150.0, 24.0],
            egui::Label::new(RichText::new(label).size(13.0).color(TEXT)),
        );
        add_control(ui)
    })
    .inner
}

fn bullet(ui: &mut egui::Ui, index: &str, title: &str, description: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(22.0, 22.0), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 11.0, ACCENT.gamma_multiply(0.18));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            index,
            egui::FontId::proportional(11.5),
            ACCENT,
        );
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(13.5).color(TEXT));
            ui.label(RichText::new(description).size(11.5).color(MUTED));
        });
    });
    ui.add_space(8.0);
}

fn step_dots(ui: &mut egui::Ui, current: usize, total: usize) {
    ui.horizontal(|ui| {
        for index in 0..total {
            let active = index <= current;
            let width = if index == current { 26.0 } else { 8.0 };
            let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), Sense::hover());
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(4),
                if active { ACCENT } else { LINE },
            );
            ui.add_space(2.0);
        }
    });
}

fn nav_item(ui: &mut egui::Ui, selected: bool, label: &str, description: &str) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::click());
    let fill = if selected {
        ACCENT.gamma_multiply(0.18)
    } else if response.hovered() {
        CARD_HOVER
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), fill);
    if selected {
        painter.rect_filled(
            Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 11.0), Vec2::new(3.0, 24.0)),
            CornerRadius::same(2),
            ACCENT,
        );
    }
    painter.text(
        rect.left_top() + Vec2::new(14.0, 7.0),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::proportional(13.5),
        if selected {
            TEXT
        } else {
            Color32::from_rgb(198, 208, 224)
        },
    );
    painter.text(
        rect.left_top() + Vec2::new(14.0, 26.0),
        egui::Align2::LEFT_TOP,
        description,
        egui::FontId::proportional(10.5),
        MUTED,
    );
    response
}

/// A full-width selectable row: the radio-button replacement used for hotkeys and backends.
fn choice_row(ui: &mut egui::Ui, selected: bool, label: &str, description: &str) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(11),
        if selected {
            ACCENT.gamma_multiply(0.16)
        } else if response.hovered() {
            CARD_HOVER
        } else {
            Color32::from_rgb(24, 30, 42)
        },
    );
    painter.rect_stroke(
        rect,
        CornerRadius::same(11),
        Stroke::new(1.0, if selected { ACCENT } else { LINE }),
        StrokeKind::Inside,
    );
    let marker = rect.left_center() + Vec2::new(20.0, 0.0);
    painter.circle_stroke(
        marker,
        7.0,
        Stroke::new(
            1.4,
            if selected {
                ACCENT
            } else {
                Color32::from_rgb(80, 92, 112)
            },
        ),
    );
    if selected {
        painter.circle_filled(marker, 3.6, ACCENT);
    }
    painter.text(
        rect.left_top() + Vec2::new(40.0, 10.0),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::proportional(13.5),
        TEXT,
    );
    painter.text(
        rect.left_top() + Vec2::new(40.0, 29.0),
        egui::Align2::LEFT_TOP,
        description,
        egui::FontId::proportional(11.0),
        MUTED,
    );
    response
}

fn toggle_row(ui: &mut egui::Ui, value: &mut bool, label: &str, description: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.set_min_height(38.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(label).size(13.5).color(TEXT));
            ui.label(RichText::new(description).size(11.0).color(MUTED));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            changed = toggle_switch(ui, value);
        });
    });
    changed
}

fn toggle_switch(ui: &mut egui::Ui, value: &mut bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(42.0, 24.0), Sense::click());
    if response.clicked() {
        *value = !*value;
    }
    let progress = ui.ctx().animate_bool_with_time(response.id, *value, 0.12);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(12),
        Color32::from_rgb(52, 62, 80).lerp_to_gamma(ACCENT, progress),
    );
    let left = rect.left() + 12.0;
    let right = rect.right() - 12.0;
    painter.circle_filled(
        egui::Pos2::new(left + (right - left) * progress, rect.center().y),
        9.0,
        Color32::from_rgb(244, 247, 252),
    );
    response.clicked()
}

fn pill_button(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::proportional(12.5),
        if selected { Color32::WHITE } else { TEXT },
    );
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 26.0, 32.0), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(16),
        if selected {
            ACCENT
        } else if response.hovered() {
            CARD_HOVER
        } else {
            Color32::from_rgb(24, 30, 42)
        },
    );
    painter.galley(rect.center() - galley.size() * 0.5, galley, TEXT);
    response
}

fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [148.0, 40.0],
        egui::Button::new(
            RichText::new(label)
                .size(13.5)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(ACCENT)
        .corner_radius(CornerRadius::same(10)),
    )
}

fn ghost_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [110.0, 38.0],
        egui::Button::new(RichText::new(label).size(13.0).color(MUTED))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(CornerRadius::same(10)),
    )
}

fn capability_card(
    ui: &mut egui::Ui,
    capabilities: &platform::Capabilities,
    input_device: Option<&str>,
) {
    card(
        ui,
        Some(("运行环境", "启动时检测，不支持的能力会安全降级")),
        |ui| {
            ui.columns(3, |columns| {
                capability_badge(&mut columns[0], "全局快捷键", capabilities.global_ptt);
                capability_badge(&mut columns[1], "浮层定位", capabilities.overlay_position);
                capability_badge(&mut columns[2], "自动插入", capabilities.synthetic_paste);
            });
            ui.add_space(10.0);
            match input_device {
                Some(name) => status_line(ui, OK, &format!("麦克风：{name}")),
                None => status_line(ui, BAD, "没有检测到可用麦克风"),
            }
            if let Some(text) = capabilities.permission_hint {
                ui.add_space(6.0);
                status_line(ui, WARN, text);
            }
        },
    );
}

fn capability_badge(ui: &mut egui::Ui, name: &str, capability: platform::Capability) {
    let color = match capability {
        platform::Capability::Available => OK,
        platform::Capability::PermissionRequired => WARN,
        platform::Capability::Degraded => BAD,
    };
    ui.vertical(|ui| {
        ui.label(RichText::new(name).size(11.0).color(MUTED));
        ui.label(
            RichText::new(capability.label())
                .size(13.0)
                .strong()
                .color(color),
        );
    });
}

fn status_line(ui: &mut egui::Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 8.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(RichText::new(text).size(11.5).color(MUTED));
    });
}

fn model_status_row(ui: &mut egui::Ui, runtime: &Runtime, settings: &ConfigFile) {
    match runtime.status() {
        EngineStatus::Ready => status_line(ui, OK, "识别引擎已就绪，可以直接开始说话"),
        EngineStatus::Loading => status_line(ui, WARN, "正在加载识别模型，第一次会慢一些…"),
        EngineStatus::Failed(error) => status_line(ui, BAD, &format!("加载失败：{error}")),
    }

    let backend = settings.asr_backend.as_deref().unwrap_or("sense-voice");
    let expected: &str = match backend {
        "funasr-nano" => settings
            .funasr_encoder_adaptor
            .as_deref()
            .unwrap_or(crate::DEFAULT_FUNASR_ENCODER_ADAPTOR),
        _ => settings.model.as_deref().unwrap_or(crate::DEFAULT_MODEL),
    };
    if !Path::new(expected).exists() {
        ui.add_space(4.0);
        status_line(ui, WARN, &format!("找不到模型文件：{expected}"));
    }
}
