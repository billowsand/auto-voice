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

    let stream_config = device.default_input_config()?;
    let sample_rate = stream_config.sample_rate().0;
    let channels = stream_config.channels() as usize;

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

    // ── rdev 全局键盘钩子（独立线程，listen() 永不返回）─────────────────────
    {
        let pressed = pressed.clone();
        let recording = recording.clone();
        let running = running.clone();
        let ptt_keys = ptt_keys.clone();
        let osd_kb = cfg.osd.clone();

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
                    recording.store(true, Ordering::SeqCst);
                    if !use_osd {
                        eprint!("\r🔴 录音中...                    ");
                    }
                    if let Some(ref o) = osd_kb {
                        o.set_level(0.0);
                        o.set_recording();
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

    // ── CPAL 持续采集 ─────────────────────────────────────────────────────────
    let stream = {
        let err_fn = |e| tracing::error!("Audio stream error: {}", e);
        device.build_input_stream(
            &stream_config.into(),
            move |data: &[f32], _| {
                let _ = audio_tx.try_send(data.to_vec());
            },
            err_fn,
            None,
        )?
    };
    stream.play()?;

    // ── 主循环 ────────────────────────────────────────────────────────────────
    let mut speech_buf: Vec<f32> = Vec::new();
    let mut was_recording = false;

    while running.load(Ordering::SeqCst) {
        let chunk = match audio_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let now_rec = recording.load(Ordering::SeqCst);
                if was_recording && !now_rec && !speech_buf.is_empty() {
                    if let Some(ref o) = cfg.osd {
                        o.set_level(0.0);
                    }
                    process_and_paste(&speech_buf, sample_rate, channels as u16, asr, cfg);
                    speech_buf.clear();
                    if !use_osd {
                        eprint!("\r✅ 已粘贴                       \n");
                    }
                    if let Some(ref o) = cfg.osd {
                        o.set_done();
                    }
                }
                was_recording = now_rec;
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let now_rec = recording.load(Ordering::SeqCst);
        let energy = rms_energy(&chunk, channels);

        if now_rec {
            if let Some(ref o) = cfg.osd {
                o.set_level(normalize_osd_level(energy, cfg.energy_threshold));
            }
            speech_buf.extend_from_slice(&chunk);
        } else if was_recording {
            if let Some(ref o) = cfg.osd {
                o.set_level(0.0);
            }
            speech_buf.extend_from_slice(&chunk);
            process_and_paste(&speech_buf, sample_rate, channels as u16, asr, cfg);
            speech_buf.clear();
            if !use_osd {
                eprint!("\r✅ 已粘贴                       \n");
            }
            if let Some(ref o) = cfg.osd {
                o.set_done();
            }
        }

        was_recording = now_rec;
    }

    Ok(())
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
