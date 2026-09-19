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

/// Shared design tokens. Keeping them in one value makes every manually painted egui component
/// follow the same theme, including cards and controls that do not use egui's widget visuals.
#[derive(Clone, Copy)]
struct ThemeTokens {
    bg: Color32,
    rail: Color32,
    card: Color32,
    card_hover: Color32,
    line: Color32,
    text: Color32,
    muted: Color32,
    accent: Color32,
    ok: Color32,
    warn: Color32,
    bad: Color32,
}

impl ThemeTokens {
    fn for_theme(theme: config::UiTheme) -> Self {
        match theme {
            config::UiTheme::MorningPorcelain => Self {
                bg: Color32::from_rgb(246, 247, 251),
                rail: Color32::from_rgb(238, 241, 247),
                card: Color32::from_rgb(255, 255, 255),
                card_hover: Color32::from_rgb(239, 243, 252),
                line: Color32::from_rgb(218, 224, 235),
                text: Color32::from_rgb(25, 32, 45),
                muted: Color32::from_rgb(101, 115, 138),
                accent: Color32::from_rgb(102, 118, 232),
                ok: Color32::from_rgb(22, 160, 133),
                warn: Color32::from_rgb(190, 130, 46),
                bad: Color32::from_rgb(214, 77, 97),
            },
            config::UiTheme::GraphiteFocus => Self {
                bg: Color32::from_rgb(24, 25, 28),
                rail: Color32::from_rgb(28, 29, 33),
                card: Color32::from_rgb(37, 39, 44),
                card_hover: Color32::from_rgb(47, 49, 56),
                line: Color32::from_rgb(61, 64, 72),
                text: Color32::from_rgb(229, 231, 235),
                muted: Color32::from_rgb(155, 161, 173),
                accent: Color32::from_rgb(136, 184, 255),
                ok: Color32::from_rgb(72, 201, 158),
                warn: Color32::from_rgb(226, 169, 79),
                bad: Color32::from_rgb(239, 101, 120),
            },
            config::UiTheme::DeepSeaAurora => Self {
                bg: Color32::from_rgb(13, 17, 24),
                rail: Color32::from_rgb(16, 21, 30),
                card: Color32::from_rgb(21, 27, 38),
                card_hover: Color32::from_rgb(26, 33, 46),
                line: Color32::from_rgb(38, 48, 65),
                text: Color32::from_rgb(237, 241, 248),
                muted: Color32::from_rgb(133, 147, 169),
                accent: Color32::from_rgb(88, 132, 240),
                ok: Color32::from_rgb(76, 211, 155),
                warn: Color32::from_rgb(235, 176, 91),
                bad: Color32::from_rgb(255, 104, 125),
            },
        }
    }
}

const THEME_DATA_ID: &str = "auto-voice-theme-tokens";

fn theme_tokens(ctx: &egui::Context) -> ThemeTokens {
    ctx.data(|data| {
        data.get_temp::<ThemeTokens>(Id::new(THEME_DATA_ID))
            .unwrap_or_else(|| ThemeTokens::for_theme(config::UiTheme::DeepSeaAurora))
    })
}

fn theme_for_ui(ui: &egui::Ui) -> ThemeTokens {
    theme_tokens(ui.ctx())
}

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
        install_theme(&creation.egui_ctx, settings.ui_theme());
        osd.attach_context(&creation.egui_ctx);
        osd.set_theme(settings.ui_theme());

        let exit_requested = Arc::new(AtomicBool::new(false));
        let exit_for_handler = Arc::clone(&exit_requested);
        let context = creation.egui_ctx.clone();

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
                install_theme(&self.ui_context, self.settings.ui_theme());
                self.osd.set_theme(self.settings.ui_theme());
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
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(false));
    }

    // ── Wizard ───────────────────────────────────────────────────────────────

    fn wizard_ui(&mut self, ui: &mut egui::Ui, step: usize) {
        page(ui, |ui| {
            ui.add_space(6.0);
            wizard_progress(ui, step);
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
        let input_device = self.effective_input_device_name();
        ui.columns(2, |columns| {
            card(
                &mut columns[0],
                Some(("三步开始使用", "按住、说话、松开")),
                |ui| {
                    bullet(ui, "1", "按住快捷键", "浮层弹出并开始录音");
                    bullet(ui, "2", "直接说话", "实时显示已经识别的内容");
                    bullet(ui, "3", "松开按键", "文字自动插入当前输入框");
                },
            );
            card(
                &mut columns[1],
                Some(("系统准备情况", "启动时会自动检测")),
                |ui| {
                    capability_badge(ui, "全局快捷键", self.capabilities.global_ptt);
                    ui.add_space(8.0);
                    capability_badge(ui, "浮层定位", self.capabilities.overlay_position);
                    ui.add_space(8.0);
                    capability_badge(ui, "自动插入", self.capabilities.synthetic_paste);
                    ui.add_space(10.0);
                    match input_device.as_deref() {
                        Some(name) => {
                            status_line(ui, theme_for_ui(ui).ok, &format!("麦克风：{name}"))
                        }
                        None => status_line(ui, theme_for_ui(ui).bad, "没有检测到可用麦克风"),
                    }
                    if let Some(text) = self.capabilities.permission_hint {
                        ui.add_space(6.0);
                        status_line(ui, theme_for_ui(ui).warn, text);
                    }
                },
            );
        });
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
        let mut changed = false;
        ui.columns(2, |columns| {
            card(
                &mut columns[0],
                Some(("识别模型", "本地运行，第一次加载需要几秒")),
                |ui| {
                    model_status_row(ui, &self.runtime, &self.settings);
                    ui.add_space(8.0);
                    hint(ui, "模型切换不需要重启程序");
                },
            );
            card(
                &mut columns[1],
                Some(("文本优化", "可选：用本地 LM Studio 顺一遍语句")),
                |ui| {
                    let mut polish = !self.settings.no_llm.unwrap_or(false);
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
        });
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
        let t = theme_for_ui(ui);
        egui::Panel::left("auto-voice-nav")
            .exact_size(208.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t.rail).inner_margin(Margin::same(14)))
            .show(ui, |ui| self.nav_ui(ui));

        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(t.bg)
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
        let t = theme_for_ui(ui);
        ui.add_space(6.0);
        ui.label(
            RichText::new("AUTO VOICE")
                .size(11.0)
                .strong()
                .color(t.accent),
        );
        ui.label(RichText::new("语音输入").size(19.0).strong().color(t.text));
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
                    .color(t.muted.gamma_multiply(0.62)),
            );
            ui.label(RichText::new("配置文件").size(10.0).color(t.muted));
            ui.add_space(10.0);
            if ghost_button(ui, "收进托盘").clicked() {
                let ctx = ui.ctx().clone();
                self.hide_window(&ctx);
            }
        });
    }

    fn status_header(&mut self, ui: &mut egui::Ui) {
        let t = theme_for_ui(ui);
        let live = self.runtime.live();
        let (color, label) = match self.runtime.status() {
            EngineStatus::Ready => (t.ok, "识别引擎就绪".to_owned()),
            EngineStatus::Loading => (t.warn, "正在加载识别模型…".to_owned()),
            EngineStatus::Failed(_) => (t.bad, "识别引擎未就绪".to_owned()),
        };
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, 9.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.5, color);
            ui.label(RichText::new(label).size(13.0).color(t.text));
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
            ui.label(RichText::new(error).size(11.5).color(t.bad));
        }
    }

    fn overview_section(&mut self, ui: &mut egui::Ui) {
        let live = self.runtime.live();
        let hotkey = config::describe_ptt_key(&live.ptt_key);

        heading(
            ui,
            "让输入跟上思考",
            "按住快捷键开始说话，松开后自动插入文字",
        );
        ui.add_space(16.0);
        card(
            ui,
            Some(("怎么用", "任何可以打字的地方都能用")),
            |ui| {
                ui.horizontal(|ui| {
                    overview_step(ui, "1", &format!("按住 {hotkey}"), "浮层弹出并开始录音");
                    overview_step(ui, "2", "直接说话", "实时显示已经识别的内容");
                    overview_step(ui, "3", "松开按键", "文字自动插入光标处");
                });
            },
        );
        ui.add_space(12.0);
        let input_device = self.effective_input_device_name();
        ui.columns(3, |columns| {
            card(
                &mut columns[0],
                Some(("运行环境", "当前设备能力")),
                |ui| {
                    capability_badge(ui, "全局快捷键", self.capabilities.global_ptt);
                    ui.add_space(8.0);
                    capability_badge(ui, "浮层定位", self.capabilities.overlay_position);
                    ui.add_space(8.0);
                    capability_badge(ui, "自动插入", self.capabilities.synthetic_paste);
                },
            );
            card(
                &mut columns[1],
                Some(("识别模型", "切换模型不需要重启")),
                |ui| {
                    model_status_row(ui, &self.runtime, &self.settings);
                    ui.add_space(8.0);
                    let backend = self
                        .settings
                        .asr_backend
                        .as_deref()
                        .unwrap_or("sense-voice");
                    ui.label(
                        RichText::new(if backend == "funasr-nano" {
                            "FunASR Nano"
                        } else {
                            "SenseVoice"
                        })
                        .size(15.0)
                        .strong()
                        .color(theme_for_ui(ui).text),
                    );
                    hint(ui, "本地 · 低延迟 · 自动加载");
                },
            );
            card(
                &mut columns[2],
                Some(("麦克风", "按住说话时使用")),
                |ui| {
                    let t = theme_for_ui(ui);
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(34.0, 34.0), Sense::hover());
                    ui.painter()
                        .circle_filled(rect.center(), 17.0, t.accent.gamma_multiply(0.16));
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "♩",
                        egui::FontId::proportional(20.0),
                        t.accent,
                    );
                    ui.add_space(6.0);
                    match input_device.as_deref() {
                        Some(name) => status_line(ui, t.ok, name),
                        None => status_line(ui, t.bad, "没有检测到可用麦克风"),
                    }
                    hint(ui, "将在下一次按住快捷键时使用");
                },
            );
        });
    }

    fn input_section(&mut self, ui: &mut egui::Ui) {
        let t = theme_for_ui(ui);
        let mut changed = false;
        let mut refresh_devices = false;
        heading(ui, "说话方式", "决定何时倾听，以及浮层如何出现");
        ui.add_space(16.0);
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
                        t.warn,
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
            ui.horizontal(|ui| {
                status_line(ui, t.ok, "屏幕中央");
                ui.label(
                    RichText::new("在当前屏幕的可用区域居中显示")
                        .size(12.0)
                        .color(t.text),
                );
            });
            ui.add_space(8.0);
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
        heading(ui, "识别模型", "选择更适合你的本地识别引擎");
        ui.add_space(16.0);
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
        heading(ui, "文本优化", "让口语转写更自然，但始终由你掌控");
        ui.add_space(16.0);
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
        } else {
            ui.add_space(12.0);
            let t = theme_for_ui(ui);
            card(
                ui,
                Some(("LM Studio", "当前未启用文本优化")),
                |ui| {
                    ui.add_enabled_ui(false, |ui| {
                        status_line(ui, t.muted, "启用 LLM 纠错后，这里可以配置本地服务");
                        hint(ui, "关闭时会直接插入原始识别结果，速度更快");
                    });
                },
            );
        }
        if changed {
            self.touched();
        }
    }

    fn appearance_section(&mut self, ui: &mut egui::Ui) {
        let t = theme_for_ui(ui);
        let mut changed = false;
        let mut selected_theme = None;
        let current_theme = self.settings.ui_theme();
        let mut remove = None;
        let mut move_font = None;
        let mut restore_defaults = false;
        let mut add_family = None;

        heading(ui, "外观", "让界面融入你的工作环境");
        ui.add_space(16.0);

        card(
            ui,
            Some(("界面主题", "设置页与语音悬浮窗会立即同步")),
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (theme, _key, _label) in config::UiTheme::ALL {
                        let label = theme.label();
                        let description = match theme {
                            config::UiTheme::DeepSeaAurora => "深色默认 · 沉浸、专注、高对比",
                            config::UiTheme::MorningPorcelain => "浅色模式 · 明亮、通透、日间友好",
                            config::UiTheme::GraphiteFocus => "中性灰 · 低调、专业、减少干扰",
                        };
                        if theme_tile(ui, theme, current_theme == theme, label, description) {
                            selected_theme = Some(theme);
                        }
                        ui.add_space(8.0);
                    }
                });
            },
        );

        if let Some(theme) = selected_theme {
            self.settings.ui_theme = Some(theme.key().to_owned());
            changed = true;
        }

        ui.add_space(12.0);

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
                                    .color(t.muted),
                            );
                            ui.label(
                                RichText::new(if installed {
                                    family.clone()
                                } else {
                                    format!("{family}（未安装）")
                                })
                                .size(13.0)
                                .color(if installed {
                                    t.text
                                } else {
                                    t.warn
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
                        .color(t.muted),
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
                                            .color(if already_selected { t.ok } else { t.text }),
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
        let t = theme_for_ui(ui);
        let Some(toast) = &self.toast else { return };
        let age = toast.at.elapsed();
        if age >= TOAST_VISIBLE_FOR {
            self.toast = None;
            return;
        }
        let fade = ((TOAST_VISIBLE_FOR - age).as_secs_f32() / 0.4).min(1.0);
        let color = if toast.error { t.bad } else { t.ok };
        let anchor = ui.max_rect();
        egui::Area::new(Id::new("auto-voice-toast"))
            .fixed_pos(anchor.left_bottom() + Vec2::new(24.0, -56.0))
            .interactable(false)
            .show(ui.ctx(), |ui| {
                Frame::new()
                    .fill(t.card.gamma_multiply(fade))
                    .stroke(Stroke::new(1.0, color.gamma_multiply(0.55 * fade)))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(14, 9))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(&toast.message)
                                .size(12.5)
                                .color(t.text.gamma_multiply(fade)),
                        );
                    });
            });
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }
}

impl eframe::App for DesktopApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested || self.runtime.shutdown_requested() {
            self.flush_pending_save();
            self.runtime.request_shutdown();
            self.exit_requested.store(true, Ordering::SeqCst);
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let monitor_size = ui.ctx().input(|input| input.viewport().monitor_size);
        let visible = self.osd.native_surface_visible();
        // Keep the click-through surface alive even while hidden: recreating it on every
        // dictation would steal focus from the window the text is about to be pasted into.
        let osd_handle = self.osd.clone();
        ui.ctx().show_viewport_deferred(
            osd::viewport_id(),
            osd::viewport_builder(monitor_size, visible),
            move |ui, _class| osd::draw(ui, &osd_handle),
        );

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
            .rect_filled(ui.max_rect(), CornerRadius::ZERO, theme_tokens(ui.ctx()).bg);
        window_chrome(ui);
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
    context.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(true));
    context.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
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
        .with_inner_size(Vec2::new(1080.0, 720.0))
        .with_min_inner_size(Vec2::new(940.0, 620.0))
        .with_decorations(false)
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

/// Branded, client-side window chrome. The native title bar is disabled so the settings window,
/// first-run wizard, and future themed surfaces share the same visual language on Windows.
fn window_chrome(ui: &mut egui::Ui) {
    let t = theme_for_ui(ui);
    let height = 48.0;
    let button_width = 40.0 * 3.0;
    ui.set_min_height(height);
    ui.horizontal(|ui| {
        ui.set_height(height);
        let drag_width = (ui.available_width() - button_width).max(160.0);
        let (drag_rect, drag_response) =
            ui.allocate_exact_size(Vec2::new(drag_width, height), Sense::click_and_drag());
        let painter = ui.painter();

        // Small waveform mark, deliberately drawn instead of relying on an image asset so it
        // follows the active accent colour in every theme.
        let logo_left = drag_rect.left() + 14.0;
        let logo_center = drag_rect.center().y;
        for (offset, bar_height) in [
            (0.0, 10.0),
            (5.0, 18.0),
            (10.0, 26.0),
            (15.0, 15.0),
            (20.0, 8.0),
        ] {
            painter.line_segment(
                [
                    egui::Pos2::new(logo_left + offset, logo_center - bar_height * 0.5),
                    egui::Pos2::new(logo_left + offset, logo_center + bar_height * 0.5),
                ],
                Stroke::new(2.2, t.accent),
            );
        }
        painter.text(
            egui::Pos2::new(logo_left + 34.0, drag_rect.top() + 10.0),
            egui::Align2::LEFT_TOP,
            "Auto Voice",
            egui::FontId::proportional(14.0),
            t.text,
        );
        painter.text(
            egui::Pos2::new(logo_left + 34.0, drag_rect.top() + 28.0),
            egui::Align2::LEFT_TOP,
            "本地语音输入",
            egui::FontId::proportional(10.0),
            t.muted,
        );

        if drag_response.drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if drag_response.double_clicked() {
            let maximized = ui
                .ctx()
                .input(|input| input.viewport().maximized.unwrap_or(false));
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if chrome_button(ui, ChromeButton::Close, t).clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }
            let maximized = ui
                .ctx()
                .input(|input| input.viewport().maximized.unwrap_or(false));
            if chrome_button(
                ui,
                if maximized {
                    ChromeButton::Restore
                } else {
                    ChromeButton::Maximize
                },
                t,
            )
            .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            }
            if chrome_button(ui, ChromeButton::Minimize, t).clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
            }
        });
    });
}

#[derive(Clone, Copy)]
enum ChromeButton {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn chrome_button(ui: &mut egui::Ui, button: ChromeButton, t: ThemeTokens) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(40.0, 48.0), Sense::click());
    let fill = if response.hovered() {
        if matches!(button, ChromeButton::Close) {
            t.bad.gamma_multiply(0.18)
        } else {
            t.card_hover
        }
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    let stroke = Stroke::new(
        1.4,
        if matches!(button, ChromeButton::Close) && response.hovered() {
            t.bad
        } else {
            t.muted
        },
    );
    let center = rect.center();
    match button {
        ChromeButton::Minimize => {
            ui.painter().line_segment(
                [center + Vec2::new(-6.0, 4.0), center + Vec2::new(6.0, 4.0)],
                stroke,
            );
        }
        ChromeButton::Maximize => {
            ui.painter().rect_stroke(
                Rect::from_center_size(center, Vec2::splat(10.0)),
                CornerRadius::same(1),
                stroke,
                StrokeKind::Inside,
            );
        }
        ChromeButton::Restore => {
            ui.painter().rect_stroke(
                Rect::from_center_size(center + Vec2::new(2.0, -2.0), Vec2::splat(8.0)),
                CornerRadius::same(1),
                stroke,
                StrokeKind::Inside,
            );
            ui.painter().rect_stroke(
                Rect::from_center_size(center + Vec2::new(-2.0, 2.0), Vec2::splat(8.0)),
                CornerRadius::same(1),
                stroke,
                StrokeKind::Inside,
            );
        }
        ChromeButton::Close => {
            ui.painter().line_segment(
                [center + Vec2::new(-5.0, -5.0), center + Vec2::new(5.0, 5.0)],
                stroke,
            );
            ui.painter().line_segment(
                [center + Vec2::new(5.0, -5.0), center + Vec2::new(-5.0, 5.0)],
                stroke,
            );
        }
    }
    response
}

fn install_theme(ctx: &egui::Context, theme: config::UiTheme) {
    let tokens = ThemeTokens::for_theme(theme);
    ctx.data_mut(|data| data.insert_temp(Id::new(THEME_DATA_ID), tokens));
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 9.0);
    style.spacing.button_padding = Vec2::new(14.0, 8.0);
    style.spacing.slider_width = 190.0;
    style.visuals.dark_mode = !matches!(theme, config::UiTheme::MorningPorcelain);
    style.visuals.panel_fill = tokens.bg;
    style.visuals.window_fill = tokens.card;
    style.visuals.extreme_bg_color = tokens.rail;
    style.visuals.selection.bg_fill = tokens.accent.gamma_multiply(0.45);
    style.visuals.widgets.inactive.bg_fill = tokens.card;
    style.visuals.widgets.inactive.fg_stroke.color = tokens.text;
    style.visuals.widgets.hovered.bg_fill = tokens.card_hover;
    style.visuals.widgets.hovered.fg_stroke.color = tokens.text;
    style.visuals.widgets.active.bg_fill = tokens.accent;
    style.visuals.widgets.active.fg_stroke.color = tokens.text;
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
    let t = theme_for_ui(ui);
    ui.label(RichText::new(title).size(26.0).strong().color(t.text));
    ui.add_space(4.0);
    ui.label(RichText::new(subtitle).size(13.0).color(t.muted));
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(theme_for_ui(ui).muted));
}

fn chip(ui: &mut egui::Ui, text: &str) {
    let t = theme_for_ui(ui);
    Frame::new()
        .fill(t.card)
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.5).color(t.muted));
        });
}

fn card(ui: &mut egui::Ui, title: Option<(&str, &str)>, add_contents: impl FnOnce(&mut egui::Ui)) {
    let t = theme_for_ui(ui);
    Frame::new()
        .fill(t.card)
        .stroke(Stroke::new(1.0, t.line))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some((title, subtitle)) = title {
                ui.label(RichText::new(title).size(15.0).strong().color(t.text));
                ui.label(RichText::new(subtitle).size(11.5).color(t.muted));
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
    let t = theme_for_ui(ui);
    ui.horizontal(|ui| {
        ui.set_min_height(34.0);
        ui.add_sized(
            [150.0, 24.0],
            egui::Label::new(RichText::new(label).size(13.0).color(t.text)),
        );
        add_control(ui)
    })
    .inner
}

fn overview_step(ui: &mut egui::Ui, index: &str, title: &str, description: &str) {
    let t = theme_for_ui(ui);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(34.0, 46.0), Sense::hover());
        ui.painter().circle_filled(
            rect.center_top() + Vec2::new(0.0, 15.0),
            15.0,
            t.accent.gamma_multiply(0.18),
        );
        ui.painter().circle_stroke(
            rect.center_top() + Vec2::new(0.0, 15.0),
            15.0,
            Stroke::new(1.0, t.accent.gamma_multiply(0.75)),
        );
        ui.painter().text(
            rect.center_top() + Vec2::new(0.0, 15.0),
            egui::Align2::CENTER_CENTER,
            index,
            egui::FontId::proportional(13.0),
            t.accent,
        );
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(13.0).strong().color(t.text));
            ui.label(RichText::new(description).size(10.5).color(t.muted));
        });
    });
}

fn bullet(ui: &mut egui::Ui, index: &str, title: &str, description: &str) {
    let t = theme_for_ui(ui);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(22.0, 22.0), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 11.0, t.accent.gamma_multiply(0.18));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            index,
            egui::FontId::proportional(11.5),
            t.accent,
        );
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(13.5).color(t.text));
            ui.label(RichText::new(description).size(11.5).color(t.muted));
        });
    });
    ui.add_space(8.0);
}

fn wizard_progress(ui: &mut egui::Ui, current: usize) {
    let t = theme_for_ui(ui);
    let steps = [
        ("欢迎", "了解用法"),
        ("快捷键", "选择按键"),
        ("完成", "开始使用"),
    ];
    ui.horizontal(|ui| {
        for (index, (label, description)) in steps.into_iter().enumerate() {
            ui.vertical(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(30.0, 30.0), Sense::hover());
                let active = index <= current;
                ui.painter().circle_filled(
                    rect.center(),
                    14.0,
                    if active {
                        t.accent.gamma_multiply(0.22)
                    } else {
                        t.card
                    },
                );
                ui.painter().circle_stroke(
                    rect.center(),
                    14.0,
                    Stroke::new(1.0, if active { t.accent } else { t.line }),
                );
                let marker = if index < current {
                    "✓".to_owned()
                } else {
                    (index + 1).to_string()
                };
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    marker,
                    egui::FontId::proportional(12.0),
                    if active { t.accent } else { t.muted },
                );
                ui.label(RichText::new(label).size(11.5).color(if active {
                    t.text
                } else {
                    t.muted
                }));
                ui.label(RichText::new(description).size(9.5).color(t.muted));
            });
            if index + 1 < steps.len() {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(54.0, 30.0), Sense::hover());
                ui.painter().line_segment(
                    [
                        rect.left_center() + Vec2::new(4.0, 0.0),
                        rect.right_center() - Vec2::new(4.0, 0.0),
                    ],
                    Stroke::new(1.0, if index < current { t.accent } else { t.line }),
                );
            }
        }
    });
}

fn nav_item(ui: &mut egui::Ui, selected: bool, label: &str, description: &str) -> egui::Response {
    let t = theme_for_ui(ui);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::click());
    let fill = if selected {
        t.accent.gamma_multiply(0.18)
    } else if response.hovered() {
        t.card_hover
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), fill);
    if selected {
        painter.rect_filled(
            Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 11.0), Vec2::new(3.0, 24.0)),
            CornerRadius::same(2),
            t.accent,
        );
    }
    painter.text(
        rect.left_top() + Vec2::new(14.0, 7.0),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::proportional(13.5),
        if selected {
            t.text
        } else {
            Color32::from_rgb(198, 208, 224)
        },
    );
    painter.text(
        rect.left_top() + Vec2::new(14.0, 26.0),
        egui::Align2::LEFT_TOP,
        description,
        egui::FontId::proportional(10.5),
        t.muted,
    );
    response
}

/// A full-width selectable row: the radio-button replacement used for hotkeys and backends.
fn choice_row(ui: &mut egui::Ui, selected: bool, label: &str, description: &str) -> egui::Response {
    let t = theme_for_ui(ui);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(11),
        if selected {
            t.accent.gamma_multiply(0.16)
        } else if response.hovered() {
            t.card_hover
        } else {
            Color32::from_rgb(24, 30, 42)
        },
    );
    painter.rect_stroke(
        rect,
        CornerRadius::same(11),
        Stroke::new(1.0, if selected { t.accent } else { t.line }),
        StrokeKind::Inside,
    );
    let marker = rect.left_center() + Vec2::new(20.0, 0.0);
    painter.circle_stroke(
        marker,
        7.0,
        Stroke::new(
            1.4,
            if selected {
                t.accent
            } else {
                Color32::from_rgb(80, 92, 112)
            },
        ),
    );
    if selected {
        painter.circle_filled(marker, 3.6, t.accent);
    }
    painter.text(
        rect.left_top() + Vec2::new(40.0, 10.0),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::proportional(13.5),
        t.text,
    );
    painter.text(
        rect.left_top() + Vec2::new(40.0, 29.0),
        egui::Align2::LEFT_TOP,
        description,
        egui::FontId::proportional(11.0),
        t.muted,
    );
    response
}

fn toggle_row(ui: &mut egui::Ui, value: &mut bool, label: &str, description: &str) -> bool {
    let t = theme_for_ui(ui);
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.set_min_height(38.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(label).size(13.5).color(t.text));
            ui.label(RichText::new(description).size(11.0).color(t.muted));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            changed = toggle_switch(ui, value);
        });
    });
    changed
}

fn toggle_switch(ui: &mut egui::Ui, value: &mut bool) -> bool {
    let t = theme_for_ui(ui);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(42.0, 24.0), Sense::click());
    if response.clicked() {
        *value = !*value;
    }
    let progress = ui.ctx().animate_bool_with_time(response.id, *value, 0.12);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(12),
        t.line.lerp_to_gamma(t.accent, progress),
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
    let t = theme_for_ui(ui);
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), egui::FontId::proportional(12.5), t.text);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 26.0, 32.0), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(16),
        if selected {
            t.accent
        } else if response.hovered() {
            t.card_hover
        } else {
            Color32::from_rgb(24, 30, 42)
        },
    );
    painter.galley(rect.center() - galley.size() * 0.5, galley, t.text);
    response
}

fn theme_tile(
    ui: &mut egui::Ui,
    theme: config::UiTheme,
    selected: bool,
    label: &str,
    description: &str,
) -> bool {
    let active = theme_for_ui(ui);
    let preview = ThemeTokens::for_theme(theme);
    let width = ((ui.available_width() - 16.0) / 3.0).max(150.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 112.0), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(12), preview.bg);
    painter.rect_stroke(
        rect,
        CornerRadius::same(12),
        Stroke::new(
            1.2,
            if selected {
                active.accent
            } else {
                preview.line
            },
        ),
        StrokeKind::Inside,
    );

    let sample = Rect::from_min_max(
        rect.left_top() + Vec2::new(12.0, 12.0),
        rect.right_top() + Vec2::new(-12.0, 54.0),
    );
    painter.rect_filled(sample, CornerRadius::same(7), preview.rail);
    painter.rect_filled(
        Rect::from_min_max(
            sample.left_top() + Vec2::new(8.0, 8.0),
            sample.left_bottom() + Vec2::new(34.0, -8.0),
        ),
        CornerRadius::same(3),
        preview.card,
    );
    painter.rect_filled(
        Rect::from_min_max(
            sample.left_top() + Vec2::new(46.0, 11.0),
            sample.right_top() + Vec2::new(-12.0, 17.0),
        ),
        CornerRadius::same(3),
        preview.accent.gamma_multiply(0.9),
    );
    painter.rect_filled(
        Rect::from_min_max(
            sample.left_top() + Vec2::new(46.0, 25.0),
            sample.right_top() + Vec2::new(-42.0, 31.0),
        ),
        CornerRadius::same(3),
        preview.ok.gamma_multiply(0.75),
    );
    painter.text(
        rect.left_bottom() + Vec2::new(12.0, -32.0),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::proportional(12.5),
        preview.text,
    );
    painter.text(
        rect.left_bottom() + Vec2::new(12.0, -16.0),
        egui::Align2::LEFT_TOP,
        description,
        egui::FontId::proportional(9.5),
        preview.muted,
    );
    if selected {
        painter.circle_filled(
            rect.right_bottom() + Vec2::new(-16.0, -16.0),
            6.0,
            active.accent,
        );
        painter.line_segment(
            [
                rect.right_bottom() + Vec2::new(-19.0, -16.0),
                rect.right_bottom() + Vec2::new(-17.0, -14.0),
            ],
            Stroke::new(1.1, Color32::WHITE),
        );
        painter.line_segment(
            [
                rect.right_bottom() + Vec2::new(-17.0, -14.0),
                rect.right_bottom() + Vec2::new(-13.0, -19.0),
            ],
            Stroke::new(1.1, Color32::WHITE),
        );
    }
    response.clicked()
}

fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let t = theme_for_ui(ui);
    ui.add_sized(
        [148.0, 40.0],
        egui::Button::new(
            RichText::new(label)
                .size(13.5)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(t.accent)
        .corner_radius(CornerRadius::same(10)),
    )
}

fn ghost_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let t = theme_for_ui(ui);
    ui.add_sized(
        [110.0, 38.0],
        egui::Button::new(RichText::new(label).size(13.0).color(t.muted))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, t.line))
            .corner_radius(CornerRadius::same(10)),
    )
}

fn capability_badge(ui: &mut egui::Ui, name: &str, capability: platform::Capability) {
    let t = theme_for_ui(ui);
    let color = match capability {
        platform::Capability::Available => t.ok,
        platform::Capability::PermissionRequired => t.warn,
        platform::Capability::Degraded => t.bad,
    };
    ui.vertical(|ui| {
        ui.label(RichText::new(name).size(11.0).color(t.muted));
        ui.label(
            RichText::new(capability.label())
                .size(13.0)
                .strong()
                .color(color),
        );
    });
}

fn status_line(ui: &mut egui::Ui, color: Color32, text: &str) {
    let t = theme_for_ui(ui);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 8.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(RichText::new(text).size(11.5).color(t.muted));
    });
}

fn model_status_row(ui: &mut egui::Ui, runtime: &Runtime, settings: &ConfigFile) {
    let t = theme_for_ui(ui);
    match runtime.status() {
        EngineStatus::Ready => status_line(ui, t.ok, "识别引擎已就绪，可以直接开始说话"),
        EngineStatus::Loading => status_line(ui, t.warn, "正在加载识别模型，第一次会慢一些…"),
        EngineStatus::Failed(error) => status_line(ui, t.bad, &format!("加载失败：{error}")),
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
        status_line(ui, t.warn, &format!("找不到模型文件：{expected}"));
    }
}
