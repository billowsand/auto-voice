/// CapsLock / 自定义组合键 Push-To-Talk 模式。
///
/// 按住所有配置的键 → 开始录音
/// 任意一键松开   → 停止录音 → ASR → LLM 润色 → 写剪切板 → 模拟 Ctrl+V 粘贴
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::asr::AsrEngine;
use crate::audio::mic::{is_meaningful_pub, set_clipboard_pub, LiveConfig};
use crate::audio::resample::to_mono_16k;
use crate::llm;

const DEFAULT_PTT_KEY: &str = "CapsLock";

pub fn run_ptt(cfg: &LiveConfig, asr: &AsrEngine) -> Result<()> {
    // ── 解析 PTT 键 ─────────────────────────────────────────────────────────
    let ptt_spec = cfg.ptt_key.as_deref().unwrap_or(DEFAULT_PTT_KEY);
    let ptt_keys: Vec<rdev::Key> = crate::config::parse_ptt_keys(ptt_spec)
        .ok_or_else(|| anyhow::anyhow!("Invalid ptt_key: \"{}\"", ptt_spec))?;

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("No input device found"))?;

    let supported_config = device.default_input_config()?;
    let sample_rate = supported_config.sample_rate().0;
    let channels = supported_config.channels() as usize;
    // 只查询设备参数，不在这里打开输入流：麦克风按需开关（见主循环）。
    let stream_config: cpal::StreamConfig = supported_config.into();

    tracing::info!(
        "PTT Mic: {} | {}Hz {}ch | 触发键: {}",
        device.name()?,
        sample_rate,
        channels,
        ptt_spec,
    );
    println!(
        "PTT 模式 — 按住 [{}] 开始录音，松开后自动识别并粘贴",
        ptt_spec
    );
    println!("(麦克风仅在按住 [{}] 期间打开)", ptt_spec);
    println!("(Ctrl+C 退出)");
    let use_osd = cfg.osd.is_some();

    // ── 共享状态 ─────────────────────────────────────────────────────────────
    // 当前按下的键集合（由 rdev 线程维护）
    let pressed: Arc<Mutex<HashSet<rdev::Key>>> = Arc::new(Mutex::new(HashSet::new()));
    // 当前是否满足"所有 ptt_keys 都按下"
    let recording = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));

    // ── 音频 channel ─────────────────────────────────────────────────────────
    let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<f32>>(128);
    // ── pending paste signal channel ───────────────────────────────────────
    let (paste_tx, paste_rx) = mpsc::sync_channel::<()>(1);

    // ── rdev 全局键盘钩子（独立线程，listen() 永不返回）─────────────────────
    {
        let pressed = pressed.clone();
        let recording = recording.clone();
        let running = running.clone();
        let ptt_keys = ptt_keys.clone();
        let osd_kb = cfg.osd.clone();
        let paste_tx = paste_tx.clone();

        std::thread::spawn(move || {
            rdev::listen(move |event| {
                let mut set = pressed.lock().unwrap();
                match event.event_type {
                    rdev::EventType::KeyPress(k) => {
                        set.insert(k);
                    }
                    rdev::EventType::KeyRelease(k) => {
                        set.remove(&k);
                    }
                    _ => {}
                }
                // 重新评估录音状态
                let all_held = ptt_keys.iter().all(|k| set.contains(k));
                let was = recording.load(Ordering::SeqCst);
                if all_held && !was {
                    // 检查是否可以开始新录音（PROCESSING/DONE 时不允许）
                    let can_start = osd_kb.as_ref().map_or(true, |o| o.can_recording_start());
                    if can_start {
                        recording.store(true, Ordering::SeqCst);
                        if !use_osd {
                            eprint!("\r🔴 录音中...                    ");
                        }
                        if let Some(ref o) = osd_kb {
                            o.set_level(0.0);
                            o.set_recording();
                        }
                    } else {
                        // 不能开始新录音，标记 pending_paste
                        let _ = paste_tx.try_send(());
                    }
                } else if !all_held && was {
                    recording.store(false, Ordering::SeqCst);
                    if !use_osd {
                        eprint!("\r⏳ 识别中...                    ");
                    }
                    if let Some(ref o) = osd_kb {
                        o.set_level(0.0);
                        o.set_processing();
                    }
                }

                if !running.load(Ordering::SeqCst) {
                    panic!("rdev stop");
                }
            })
            .ok();
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
    let mut stream: Option<cpal::Stream> = None;
    let mut speech_buf: Vec<f32> = Vec::new();
    let mut was_recording = false;
    let mut pending_paste = false; // 追踪是否需要处理（松键但可能被忽略）

    while running.load(Ordering::SeqCst) {
        // 检查是否有 pending_paste 信号
        while let Ok(()) = paste_rx.try_recv() {
            pending_paste = true;
        }

        let now_rec = recording.load(Ordering::SeqCst);

        // ① 按下 PTT → 打开麦克风
        if now_rec && stream.is_none() {
            // 丢弃上一轮遗留的音频，避免混入本次录音
            while audio_rx.try_recv().is_ok() {}
            speech_buf.clear();

            match open_mic(&device, &stream_config, audio_tx.clone()) {
                Ok(s) => {
                    tracing::info!("麦克风已打开");
                    stream = Some(s);
                }
                Err(e) => {
                    tracing::error!("打开麦克风失败: {}", e);
                    recording.store(false, Ordering::SeqCst);
                    if !use_osd {
                        eprint!("\r❌ 麦克风打开失败              \n");
                    }
                    if let Some(ref o) = cfg.osd {
                        o.hide();
                    }
                    was_recording = false;
                    continue;
                }
            }
        }

        // ② 录音中 → 收音 + 更新 OSD 电平
        if now_rec {
            match audio_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => {
                    if let Some(ref o) = cfg.osd {
                        let energy = rms_energy(&chunk, channels);
                        o.set_level(normalize_osd_level(energy, cfg.energy_threshold));
                    }
                    speech_buf.extend_from_slice(&chunk);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            was_recording = true;
            continue;
        }

        // ③ 松开 PTT → 先关闭麦克风，再收回回调里已经送出的尾音
        if let Some(s) = stream.take() {
            drop(s);
            tracing::info!("麦克风已关闭");
            while let Ok(chunk) = audio_rx.try_recv() {
                speech_buf.extend_from_slice(&chunk);
            }
            if let Some(ref o) = cfg.osd {
                o.set_level(0.0);
            }
        }

        // ④ 识别 + 粘贴
        // 两种情况：1. 正常松键 2. pending_paste（松键时被忽略，现在可以处理了）
        if (was_recording || pending_paste) && !speech_buf.is_empty() {
            pending_paste = false;
            was_recording = false;
            process_and_paste(&speech_buf, sample_rate, channels as u16, asr, cfg);
            speech_buf.clear();
            if !use_osd {
                eprint!("\r✅ 已粘贴                       \n");
            }
            if let Some(ref o) = cfg.osd {
                o.set_done();
            }
            continue;
        }

        was_recording = false;
        // 空闲：麦克风已关闭，此处只是等待下一次按键。
        // 轮询间隔要短，否则会拖慢按下 PTT 到麦克风打开的响应。
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
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
    cfg: &LiveConfig,
) {
    let mono = match to_mono_16k(raw, sample_rate, channels) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Resample error: {}", e);
            return;
        }
    };

    let seg = match asr.transcribe(&mono) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("ASR error: {}", e);
            return;
        }
    };

    if !is_meaningful_pub(&seg.text) {
        return;
    }

    let final_text = if cfg.no_llm {
        seg.text
    } else {
        match llm::polish_voice_blocking(&cfg.lm_url, &cfg.lm_model, &seg.text) {
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
}

fn paste_at_cursor() {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    match Enigo::new(&Settings::default()) {
        Ok(mut enigo) => {
            let _ = enigo.key(Key::Control, Direction::Press);
            let _ = enigo.key(Key::Unicode('v'), Direction::Click);
            let _ = enigo.key(Key::Control, Direction::Release);
        }
        Err(e) => tracing::warn!("enigo init failed: {}", e),
    }
}
