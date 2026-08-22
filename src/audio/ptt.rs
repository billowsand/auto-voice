/// Push-To-Talk：按住配置的键说话，松开后转写并插入到光标处。
///
/// 按住所有配置的键 → 开始录音
/// 任意一键松开   → 停止录音 → ASR → LLM 润色 → 写剪切板 → 模拟 Ctrl+V 粘贴
///
/// 设置改动通过 [`Runtime`] 实时生效：触发键、能量阈值、LLM 开关立刻换用新值，
/// 换模型/后端则在本线程空闲时重新加载引擎，都不需要重启程序。
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::asr::AsrEngine;
use crate::audio::mic::{is_meaningful_pub, set_clipboard_pub};
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

pub fn run_ptt(runtime: &Runtime, osd: Option<OsdHandle>) -> Result<()> {
    #[cfg(target_os = "linux")]
    if crate::platform::is_wayland_session() {
        anyhow::bail!(
            "Wayland global PTT requires the XDG GlobalShortcuts Portal backend; use X11 or open Settings to review platform capabilities"
        );
    }

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("No input device found"))?;

    let supported_config = device.default_input_config()?;
    let sample_rate = supported_config.sample_rate().0;
    let channels = supported_config.channels() as usize;
    // 只查询设备参数，不在这里打开输入流：麦克风按需开关（见主循环）。
    let stream_config: cpal::StreamConfig = supported_config.into();

    let use_osd = osd.is_some();
    tracing::info!(
        "PTT Mic: {} | {}Hz {}ch | 触发键: {}",
        device.name()?,
        sample_rate,
        channels,
        runtime.live().ptt_key,
    );
    if !use_osd {
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

    // ── rdev 全局键盘钩子（独立线程）─────────────────────────────────────────
    //
    // 回调运行在 WH_KEYBOARD_LL 里：超过 LowLevelHooksTimeout（默认 300ms）Windows 会
    // 直接把钩子摘掉，整个 PTT 就此失灵。所以这里只更新按键集合与一个原子标志，
    // 开麦克风、弹浮层、查前台窗口等等一律留给主循环。
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

    // ── Ctrl+C ───────────────────────────────────────────────────────────────
    {
        let running = running.clone();
        ctrlc::set_handler(move || {
            println!("\nStopping...");
            running.store(false, Ordering::SeqCst);
        })
        .ok();
    }

    // ── 主循环 ────────────────────────────────────────────────────────────────
    // 麦克风句柄按需持有：按下 PTT 才创建输入流，松开立即 drop。
    // 空闲时进程不占用录音设备，Windows 也不会显示"正在使用麦克风"。
    let mut engine: Option<AsrEngine> = None;
    let mut stream: Option<cpal::Stream> = None;
    let mut speech_buf: Vec<f32> = Vec::new();
    // 开麦失败后等触发键松开再重试，否则会在按住期间每 10ms 重试一次。
    let mut blocked_until_release = false;

    while running.load(Ordering::SeqCst) {
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
                        engine = Some(loaded);
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
                match open_mic(&device, &stream_config, audio_tx.clone()) {
                    Ok(opened) => {
                        tracing::info!("麦克风已打开");
                        stream = Some(opened);
                        match osd {
                            Some(ref osd) => {
                                osd.set_level(0.0);
                                osd.set_recording();
                            }
                            None => eprint!("\r🔴 录音中...                    "),
                        }
                    }
                    Err(error) => {
                        tracing::error!("打开麦克风失败: {}", error);
                        blocked_until_release = true;
                        report(&osd, Outcome::Failed("麦克风打开失败，检查录音权限".into()));
                    }
                }
            }
        }

        // ② 录音中 → 收音 + 更新浮层电平
        if stream.is_some() && is_held {
            match audio_rx.recv_timeout(Duration::from_millis(30)) {
                Ok(chunk) => {
                    if let Some(ref osd) = osd {
                        let energy = rms_energy(&chunk, channels);
                        osd.set_level(normalize_osd_level(energy, tunables.energy_threshold));
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
            tracing::info!("麦克风已关闭");
            while let Ok(chunk) = audio_rx.try_recv() {
                speech_buf.extend_from_slice(&chunk);
            }
            match osd {
                Some(ref osd) => {
                    osd.set_level(0.0);
                    osd.set_processing();
                }
                None => eprint!("\r⏳ 识别中...                    "),
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
            report(&osd, outcome);
            continue;
        }

        // 空闲：麦克风已关闭，此处只是等待下一次按键。
        // 轮询间隔要短，否则会拖慢按下 PTT 到麦克风打开的响应。
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

/// 把一次听写的结果告诉用户：托盘模式走浮层，CLI 模式走 stderr。
fn report(osd: &Option<OsdHandle>, outcome: Outcome) {
    match (osd, outcome) {
        (Some(osd), Outcome::Inserted(text)) => osd.set_done(&text),
        (Some(osd), Outcome::NothingHeard) => osd.set_notice("没有听清，再按住试一次", false),
        (Some(osd), Outcome::Failed(message)) => osd.set_notice(message, true),
        (None, Outcome::Inserted(_)) => eprint!("\r✅ 已粘贴                       \n"),
        (None, Outcome::NothingHeard) => eprint!("\r🤷 没有识别到语音               \n"),
        (None, Outcome::Failed(message)) => eprint!("\r❌ {message}\n"),
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

    set_clipboard_pub(&final_text);
    std::thread::sleep(Duration::from_millis(80));
    paste_at_cursor();
    Outcome::Inserted(final_text)
}

fn paste_at_cursor() {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    match Enigo::new(&Settings::default()) {
        Ok(mut enigo) => {
            #[cfg(target_os = "macos")]
            let modifier = Key::Meta;
            #[cfg(not(target_os = "macos"))]
            let modifier = Key::Control;

            let _ = enigo.key(modifier, Direction::Press);
            let _ = enigo.key(Key::Unicode('v'), Direction::Click);
            let _ = enigo.key(modifier, Direction::Release);
        }
        Err(e) => tracing::warn!("enigo init failed: {}", e),
    }
}
