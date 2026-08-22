/// Push-To-Talk：按住配置的键说话，松开后转写并插入到光标处。
///
/// 按住所有配置的键 → 开始录音
/// 任意一键松开   → 停止录音 → ASR → LLM 润色 → 写剪切板 → 模拟 Ctrl+V 粘贴
///
/// 设置改动通过 [`Runtime`] 实时生效：触发键、能量阈值、LLM 开关立刻换用新值，
/// 换模型/后端则在本线程空闲时重新加载引擎，都不需要重启程序。
use anyhow::Result;
use cpal::traits::{DeviceTrait, StreamTrait};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::asr::AsrEngine;
use crate::audio::mic::{is_meaningful_pub, set_clipboard_pub};
use crate::audio::preview::LivePreview;
use crate::audio::resample::to_mono_16k;
use crate::llm;
use crate::osd::OsdHandle;
use crate::runtime::{EngineStatus, LiveTunables, Runtime};

/// What one dictation cycle produced, so the overlay can tell the truth about it.
enum Outcome {
    Inserted(String),
    NothingHeard,
    Failed(String),
}

/// Omarchy renders desktop notifications as compositor-native overlays. On Wayland this is
/// more reliable than an eframe child viewport: it is global across workspaces, never takes
/// keyboard focus, and cannot make the settings window miss compositor pings.
struct WaylandOsd {
    enabled: bool,
    notification_id: Option<u32>,
}

impl WaylandOsd {
    fn new(desktop_osd_unavailable: bool) -> Self {
        Self {
            enabled: cfg!(target_os = "linux")
                && crate::platform::is_wayland_session()
                && desktop_osd_unavailable,
            notification_id: None,
        }
    }

    fn show(&mut self, title: &str, body: &str, expire_ms: u32) {
        if !self.enabled {
            return;
        }

        #[cfg(target_os = "linux")]
        {
            let kind = if title.contains("正在聆听") {
                "listening"
            } else if title.contains("正在转写") {
                "processing"
            } else if title.contains("已插入") {
                "done"
            } else {
                "failed"
            };
            if crate::wayland_osd::show(kind, body).is_ok() {
                return;
            }

            // If the isolated renderer cannot start, preserve status feedback through the
            // compositor-native notification layer rather than failing the dictation cycle.
            let mut command = std::process::Command::new("notify-send");
            command.args([
                "--print-id",
                "--app-name=Auto Voice",
                "--urgency=low",
                "--icon=audio-input-microphone-symbolic",
                &format!("--expire-time={expire_ms}"),
            ]);
            if let Some(id) = self.notification_id {
                command.arg(format!("--replace-id={id}"));
            }
            match command.args([title, body]).output() {
                Ok(output) if output.status.success() => {
                    self.notification_id = String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .parse()
                        .ok()
                        .or(self.notification_id);
                }
                Ok(output) => tracing::warn!(
                    "Omarchy OSD notification failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                Err(error) => tracing::warn!("Failed to run notify-send: {error}"),
            }
        }
    }
}

pub fn run_ptt(runtime: &Runtime, osd: Option<OsdHandle>) -> Result<()> {
    #[cfg(target_os = "linux")]
    if crate::platform::is_wayland_session() {
        // Keep the worker alive so it can load/reload the ASR model and expose an honest
        // engine status in Settings. rdev listens through XWayland here, which may only see
        // keys while an X11/XWayland application has focus; the compositor binding below
        // supplies native Wayland-wide press/release events instead.
        tracing::info!("Wayland session: using compositor press/release signals for PTT");
    }

    let use_osd = osd.is_some();
    let mut wayland_osd = WaylandOsd::new(!use_osd);
    let has_visual_osd = use_osd || wayland_osd.enabled;
    tracing::info!(
        "PTT ready | 触发键: {} | 麦克风: {}",
        runtime.live().ptt_key,
        runtime.live().input_device.as_deref().unwrap_or("系统默认"),
    );
    if !has_visual_osd {
        println!(
            "PTT 模式 — 按住 [{}] 开始录音，松开后自动识别并粘贴",
            runtime.live().ptt_key
        );
        println!("(麦克风仅在按住触发键期间打开)");
        println!("(Ctrl+C 退出)");
    }

    // ── 共享状态 ─────────────────────────────────────────────────────────────
    // 触发键当前是否全部按下。键盘钩子只写这一个标志，主循环负责其余所有状态。
    let held = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));

    // ── 音频 channel ─────────────────────────────────────────────────────────
    let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<f32>>(128);

    #[cfg(target_os = "linux")]
    let _wayland_ipc = crate::platform::is_wayland_session()
        .then(|| crate::hotkey_ipc::Server::start(held.clone()))
        .transpose()?;

    // ── rdev 全局键盘钩子（独立线程）─────────────────────────────────────────
    //
    // 回调运行在 WH_KEYBOARD_LL 里：超过 LowLevelHooksTimeout（默认 300ms）Windows 会
    // 直接把钩子摘掉，整个 PTT 就此失灵。所以这里只更新按键集合与一个原子标志，
    // 开麦克风、弹浮层、查前台窗口等等一律留给主循环。
    #[cfg(not(target_os = "linux"))]
    {
        let held = held.clone();
        let running = running.clone();
        let runtime = runtime.clone();

        std::thread::spawn(move || {
            // rdev's Windows backend pumps a single `GetMessage`, so the hook dies as soon as
            // any message lands on this thread — silently, and with it the whole hotkey.
            // Reinstall instead of leaving the user with a dictation key that does nothing.
            while running.load(Ordering::SeqCst) {
                let held = held.clone();
                let running = running.clone();
                let runtime = runtime.clone();
                let mut pressed: HashSet<rdev::Key> = HashSet::new();

                let result = rdev::listen(move |event| {
                    match event.event_type {
                        rdev::EventType::KeyPress(key) => {
                            pressed.insert(key);
                        }
                        rdev::EventType::KeyRelease(key) => {
                            pressed.remove(&key);
                        }
                        _ => return,
                    }
                    // 触发键每次都从 runtime 读取，设置里改完立即生效。
                    let keys = runtime.ptt_keys();
                    held.store(
                        !keys.is_empty() && keys.iter().all(|key| pressed.contains(key)),
                        Ordering::SeqCst,
                    );

                    if !running.load(Ordering::SeqCst) {
                        panic!("rdev stop");
                    }
                });

                match result {
                    Ok(()) => tracing::warn!("Global keyboard hook stopped; reinstalling"),
                    Err(error) => {
                        tracing::error!("Global keyboard hook failed, retrying: {error:?}");
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        });
    }

    // X11 can still use rdev, but on Wayland it only sees keys delivered through XWayland.
    // Running it beside the compositor signal backend creates a split state where one backend
    // observes the press and the other misses the release.
    #[cfg(target_os = "linux")]
    if !crate::platform::is_wayland_session() {
        let held = held.clone();
        let running = running.clone();
        let runtime = runtime.clone();
        std::thread::spawn(move || listen_with_rdev(held, running, runtime));
    }

    // ── Ctrl+C ───────────────────────────────────────────────────────────────
    {
        let running = running.clone();
        let runtime = runtime.clone();
        ctrlc::set_handler(move || {
            println!("\nStopping...");
            running.store(false, Ordering::SeqCst);
            runtime.request_shutdown();
        })
        .ok();
    }

    // ── 主循环 ────────────────────────────────────────────────────────────────
    // 麦克风句柄按需持有：按下 PTT 才创建输入流，松开立即 drop。
    // 空闲时进程不占用录音设备，Windows 也不会显示"正在使用麦克风"。
    let mut engine: Option<Arc<AsrEngine>> = None;
    let mut stream: Option<cpal::Stream> = None;
    let mut active_format: Option<(u32, usize)> = None;
    let mut recording_started_at: Option<std::time::Instant> = None;
    // Runs the recogniser over the audio so far while the key is still held, so the overlay can
    // show the transcript building up. Retired the moment the key is released.
    let mut preview: Option<LivePreview> = None;
    let mut speech_buf: Vec<f32> = Vec::new();
    // 开麦失败后等触发键松开再重试，否则会在按住期间每 10ms 重试一次。
    let mut blocked_until_release = false;

    while running.load(Ordering::SeqCst) && !runtime.shutdown_requested() {
        let is_held = held.load(Ordering::SeqCst);
        if !is_held {
            blocked_until_release = false;
        }

        // ⓪ 空闲时加载/重载模型：首次启动与设置里换模型走同一条路径。
        if stream.is_none() {
            if let Some((generation, asr_config, hr_config)) = runtime.pending_asr() {
                runtime.set_status(EngineStatus::Loading);
                engine = None; // 先释放旧引擎，避免两份模型同时占内存
                match AsrEngine::new(&asr_config, Some(&hr_config)) {
                    Ok(loaded) => {
                        tracing::info!("ASR model ready");
                        engine = Some(Arc::new(loaded));
                        runtime.set_status(EngineStatus::Ready);
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        tracing::error!("Failed to load ASR model: {message}");
                        runtime.set_status(EngineStatus::Failed(message));
                    }
                }
                runtime.mark_asr_applied(generation);
            }
        }

        let tunables = runtime.live();

        // ① 按住触发键 → 开麦克风、弹浮层
        if is_held && stream.is_none() && !blocked_until_release {
            // 上一轮的结果还在浮层上时不抢跑，等它消失。
            let ready = osd.as_ref().is_none_or(|osd| osd.can_recording_start());
            if ready {
                while audio_rx.try_recv().is_ok() {} // 丢弃上一轮遗留的音频
                speech_buf.clear();
                match open_selected_mic(tunables.input_device.as_deref(), audio_tx.clone()) {
                    Ok(opened) => {
                        tracing::info!(
                            "麦克风已打开: {} | {}Hz {}ch{}",
                            opened.name,
                            opened.sample_rate,
                            opened.channels,
                            if opened.used_fallback {
                                "（所选设备不可用，已回退系统默认）"
                            } else {
                                ""
                            }
                        );
                        let sample_rate = opened.sample_rate;
                        let channels = opened.channels;
                        active_format = Some((sample_rate, channels));
                        recording_started_at = Some(std::time::Instant::now());
                        stream = Some(opened.stream);
                        match osd {
                            Some(ref osd) => {
                                osd.set_level(0.0);
                                osd.set_recording();
                                preview = tunables
                                    .live_preview
                                    .then(|| engine.clone())
                                    .flatten()
                                    .and_then(|engine| {
                                        LivePreview::start(
                                            engine,
                                            osd.clone(),
                                            sample_rate,
                                            channels as u16,
                                        )
                                    });
                            }
                            None => {
                                wayland_osd.show(
                                    "Auto Voice · 正在聆听",
                                    "松开右 Alt 后开始转写",
                                    0,
                                );
                                if !wayland_osd.enabled {
                                    eprint!("\r🔴 录音中...                    ");
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::error!("打开麦克风失败: {}", error);
                        blocked_until_release = true;
                        report(
                            &osd,
                            &mut wayland_osd,
                            Outcome::Failed("麦克风打开失败，检查录音权限".into()),
                        );
                    }
                }
            }
        }

        // ② 录音中 → 收音 + 更新浮层电平
        if stream.is_some() && is_held {
            if recording_started_at
                .is_some_and(|started| started.elapsed() >= Duration::from_secs(90))
            {
                tracing::warn!("PTT safety timeout reached; forcing release");
                held.store(false, Ordering::SeqCst);
                continue;
            }
            let channels = active_format.map_or(1, |(_, channels)| channels);
            match audio_rx.recv_timeout(Duration::from_millis(30)) {
                Ok(chunk) => {
                    if let Some(ref osd) = osd {
                        let energy = rms_energy(&chunk, channels);
                        osd.set_level(normalize_osd_level(energy, tunables.energy_threshold));
                    }
                    if let Some(ref preview) = preview {
                        preview.push(&chunk);
                    }
                    speech_buf.extend_from_slice(&chunk);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            continue;
        }

        // ③ 松开触发键 → 关麦克风，收回回调里已经送出的尾音，然后识别 + 粘贴
        if let Some(opened) = stream.take() {
            drop(opened);
            recording_started_at = None;
            tracing::info!("麦克风已关闭");
            let (sample_rate, channels) = active_format.take().unwrap_or((16_000, 1));
            // Retire the preview before the tail is collected: whatever it has already put on
            // the overlay stays there until the real transcript replaces it.
            preview = None;
            while let Ok(chunk) = audio_rx.try_recv() {
                speech_buf.extend_from_slice(&chunk);
            }
            match osd {
                Some(ref osd) => {
                    osd.set_level(0.0);
                    osd.set_processing();
                }
                None => {
                    wayland_osd.show("Auto Voice · 正在转写", "正在识别并插入活动窗口", 0);
                    if !wayland_osd.enabled {
                        eprint!("\r⏳ 识别中...                    ");
                    }
                }
            }

            let outcome = match engine.as_ref() {
                _ if speech_buf.is_empty() => Outcome::NothingHeard,
                Some(engine) => {
                    process_and_paste(&speech_buf, sample_rate, channels as u16, engine, &tunables)
                }
                None => Outcome::Failed(match runtime.status() {
                    EngineStatus::Failed(error) => format!("识别模型未就绪：{error}"),
                    _ => "识别模型仍在加载，稍后再试".to_owned(),
                }),
            };
            speech_buf.clear();
            report(&osd, &mut wayland_osd, outcome);
            continue;
        }

        // 空闲：麦克风已关闭，此处只是等待下一次按键。
        // 轮询间隔要短，否则会拖慢按下 PTT 到麦克风打开的响应。
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn listen_with_rdev(held: Arc<AtomicBool>, running: Arc<AtomicBool>, runtime: Runtime) {
    while running.load(Ordering::SeqCst) {
        let held = held.clone();
        let running = running.clone();
        let runtime = runtime.clone();
        let mut pressed: HashSet<rdev::Key> = HashSet::new();
        let result = rdev::listen(move |event| {
            match event.event_type {
                rdev::EventType::KeyPress(key) => {
                    pressed.insert(key);
                }
                rdev::EventType::KeyRelease(key) => {
                    pressed.remove(&key);
                }
                _ => return,
            }
            let keys = runtime.ptt_keys();
            held.store(
                !keys.is_empty() && keys.iter().all(|key| pressed.contains(key)),
                Ordering::SeqCst,
            );
            if !running.load(Ordering::SeqCst) {
                panic!("rdev stop");
            }
        });
        match result {
            Ok(()) => tracing::warn!("Global keyboard hook stopped; reinstalling"),
            Err(error) => {
                tracing::error!("Global keyboard hook failed, retrying: {error:?}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

/// 把一次听写的结果告诉用户：托盘模式走浮层，CLI 模式走 stderr。
fn report(osd: &Option<OsdHandle>, wayland_osd: &mut WaylandOsd, outcome: Outcome) {
    match &outcome {
        Outcome::Inserted(text) => wayland_osd.show("Auto Voice · 已插入", text, 2200),
        Outcome::NothingHeard => {
            wayland_osd.show("Auto Voice · 未识别", "没有听清，请再试一次", 2600)
        }
        Outcome::Failed(message) => wayland_osd.show("Auto Voice · 失败", message, 3500),
    }
    match (osd, outcome) {
        (Some(osd), Outcome::Inserted(text)) => osd.set_done(&text),
        (Some(osd), Outcome::NothingHeard) => osd.set_notice("没有听清，再按住试一次", false),
        (Some(osd), Outcome::Failed(message)) => osd.set_notice(message, true),
        (None, Outcome::Inserted(_)) if !wayland_osd.enabled => {
            eprint!("\r✅ 已粘贴                       \n")
        }
        (None, Outcome::NothingHeard) if !wayland_osd.enabled => {
            eprint!("\r🤷 没有识别到语音               \n")
        }
        (None, Outcome::Failed(message)) if !wayland_osd.enabled => eprint!("\r❌ {message}\n"),
        (None, _) => {}
    }
}

/// 创建并启动一路输入流。调用方 drop 返回值即关闭麦克风。
fn open_mic(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: mpsc::SyncSender<Vec<f32>>,
) -> Result<cpal::Stream> {
    let err_fn = |e| tracing::error!("Audio stream error: {}", e);
    let stream = device.build_input_stream(
        config,
        move |data: &[f32], _| {
            let _ = tx.try_send(data.to_vec());
        },
        err_fn,
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

struct OpenedMic {
    stream: cpal::Stream,
    sample_rate: u32,
    channels: usize,
    name: String,
    used_fallback: bool,
}

fn open_selected_mic(preferred: Option<&str>, tx: mpsc::SyncSender<Vec<f32>>) -> Result<OpenedMic> {
    let selection = crate::audio::select_input_device(preferred)?;
    let should_retry_default = preferred.is_some() && !selection.used_fallback;
    let selected_name = selection.name.clone();
    match open_input_device(selection, tx.clone()) {
        Ok(opened) => Ok(opened),
        Err(error) if should_retry_default => {
            tracing::warn!(
                "Failed to open selected microphone {:?}: {error:#}; trying the system default",
                selected_name
            );
            let fallback = crate::audio::select_input_device(None)?;
            if fallback.name == selected_name {
                return Err(error);
            }
            let mut opened = open_input_device(fallback, tx)?;
            opened.used_fallback = true;
            Ok(opened)
        }
        Err(error) => Err(error),
    }
}

fn open_input_device(
    selection: crate::audio::InputDeviceSelection,
    tx: mpsc::SyncSender<Vec<f32>>,
) -> Result<OpenedMic> {
    let supported_config = selection.device.default_input_config()?;
    let sample_rate = supported_config.sample_rate().0;
    let channels = supported_config.channels() as usize;
    let stream_config: cpal::StreamConfig = supported_config.into();
    let stream = open_mic(&selection.device, &stream_config, tx)?;
    Ok(OpenedMic {
        stream,
        sample_rate,
        channels,
        name: selection.name,
        used_fallback: selection.used_fallback,
    })
}

fn rms_energy(samples: &[f32], channels: usize) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let frames = samples.len() / channels;
    if frames == 0 {
        return 0.0;
    }
    let sum_sq: f32 = (0..frames)
        .map(|i| {
            let mono = (0..channels)
                .map(|c| samples[i * channels + c])
                .sum::<f32>()
                / channels as f32;
            mono * mono
        })
        .sum();
    (sum_sq / frames as f32).sqrt()
}

fn normalize_osd_level(energy: f32, threshold: f32) -> f32 {
    let floor = (threshold * 0.35).max(0.0005);
    ((energy - floor) / (0.18 - floor)).clamp(0.0, 1.0)
}

fn process_and_paste(
    raw: &[f32],
    sample_rate: u32,
    channels: u16,
    asr: &AsrEngine,
    tunables: &LiveTunables,
) -> Outcome {
    let mono = match to_mono_16k(raw, sample_rate, channels) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Resample error: {}", e);
            return Outcome::Failed("音频重采样失败".into());
        }
    };

    let seg = match asr.transcribe(&mono) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("ASR error: {}", e);
            return Outcome::Failed("识别失败，详见日志".into());
        }
    };
    tracing::info!(
        "ASR done: {:.1}s audio → {} chars",
        mono.len() as f32 / 16_000.0,
        seg.text.chars().count()
    );

    if !is_meaningful_pub(&seg.text) {
        return Outcome::NothingHeard;
    }

    let final_text = if tunables.no_llm {
        seg.text
    } else {
        match llm::polish_voice_blocking(&tunables.lm_url, &tunables.lm_model, &seg.text) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("LLM error (using raw): {}", e);
                seg.text
            }
        }
    };

    if let Err(error) = set_clipboard_pub(&final_text) {
        tracing::error!("Clipboard write failed: {error:#}");
        return Outcome::Failed("无法写入 Wayland 剪贴板".into());
    }
    std::thread::sleep(Duration::from_millis(120));
    if let Err(error) = paste_at_cursor() {
        tracing::error!("Paste dispatch failed: {error:#}");
        return Outcome::Failed("文字已复制，但无法发送粘贴快捷键".into());
    }
    Outcome::Inserted(final_text)
}

fn paste_at_cursor() -> Result<()> {
    #[cfg(target_os = "linux")]
    if crate::platform::is_wayland_session() {
        return wayland_paste_at_cursor();
    }

    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    match Enigo::new(&Settings::default()) {
        Ok(mut enigo) => {
            #[cfg(target_os = "macos")]
            let modifier = Key::Meta;
            #[cfg(not(target_os = "macos"))]
            let modifier = Key::Control;

            enigo.key(modifier, Direction::Press)?;
            enigo.key(Key::Unicode('v'), Direction::Click)?;
            enigo.key(modifier, Direction::Release)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Wayland paste delivery: pick the chord the focused application understands, then inject it.
///
/// Terminals do not paste on Ctrl+V (the shell reads it as literal-next), which is why a
/// "successful" paste used to do nothing in a terminal. Omarchy answers its universal paste
/// with Shift+Insert there; mirror that. Delivery goes through Hyprland's own synthetic key
/// state — a down now and a delayed up, exactly like Omarchy's universal clipboard binding —
/// because `sendshortcut` can leave synthetic keys repeating and `wtype` merges with whatever
/// modifier the user is still physically holding.
#[cfg(target_os = "linux")]
fn wayland_paste_at_cursor() -> Result<()> {
    let terminal = hyprland_terminal_focused();
    let (mods, key) = if terminal {
        ("SHIFT", "Insert")
    } else {
        ("CTRL", "V")
    };

    if crate::platform::is_hyprland_session() {
        let lua = format!(
            r#"hl.dispatch(hl.dsp.send_key_state({{ mods = "{mods}", key = "{key}", state = "down" }})); hl.timer(function() hl.dispatch(hl.dsp.send_key_state({{ mods = "{mods}", key = "{key}", state = "up" }})) end, {{ timeout = 50, type = "oneshot" }})"#
        );
        match std::process::Command::new("hyprctl")
            .args(["eval", &lua])
            .status()
        {
            Ok(status) if status.success() => {
                tracing::info!("Paste shortcut dispatched through Hyprland ({mods}+{key})");
                return Ok(());
            }
            Ok(status) => {
                tracing::warn!("Hyprland paste dispatch failed with {status}; trying wtype")
            }
            Err(error) => {
                tracing::warn!("hyprctl eval unavailable: {error}; trying wtype")
            }
        }
    }

    // Generic Wayland fallback: wtype speaks virtual-keyboard-unstable-v1, which wlroots
    // compositors and KWin support, so it does not depend on Hyprland at all.
    let (mod_name, key_name) = if terminal {
        ("shift", "Insert")
    } else {
        ("ctrl", "v")
    };
    match std::process::Command::new("wtype")
        .args(["-M", mod_name, "-k", key_name, "-m", mod_name])
        .status()
    {
        Ok(status) if status.success() => {
            tracing::info!("Paste shortcut dispatched through wtype");
            Ok(())
        }
        Ok(status) => anyhow::bail!("wtype paste failed with {status}"),
        Err(error) => Err(anyhow::anyhow!("failed to run wtype for paste: {error}")),
    }
}

/// True when the focused Hyprland window is a terminal emulator, where paste is Shift+Insert.
/// Query failures are conservative: the regular Ctrl+V is the right choice almost everywhere.
#[cfg(target_os = "linux")]
fn hyprland_terminal_focused() -> bool {
    if !crate::platform::is_hyprland_session() {
        return false;
    }
    let Ok(output) = std::process::Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return false;
    };
    let Some(class) = value.get("class").and_then(|class| class.as_str()) else {
        return false;
    };
    let class = class.to_ascii_lowercase();
    const TERMINALS: &[&str] = &[
        "alacritty",
        "kitty",
        "foot",
        "footclient",
        "ghostty",
        "com.mitchellh.ghostty",
        "wezterm",
        "org.wezfurlong.wezterm",
        "konsole",
        "gnome-terminal",
        "gnome-terminal-server",
        "xterm",
        "st",
        "st-256color",
        "rio",
        "contour",
        "tilix",
        "terminator",
        "yakuake",
        "tilda",
        "guake",
        "blackbox",
        "tabby",
        "hyper",
        "warp-terminal",
        "cool-retro-term",
    ];
    TERMINALS.contains(&class.as_str())
}
