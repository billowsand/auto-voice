use anyhow::Result;
use cpal::traits::{DeviceTrait, StreamTrait};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::asr::AsrEngine;
use crate::audio::resample::to_mono_16k;
use crate::llm;

/// Minimum speech duration before a segment is sent to ASR (in device samples, 16kHz equivalent).
const MIN_SPEECH_SAMPLES: usize = 8000; // ~500ms

/// Maximum segment length before forcing a flush.
const MAX_SEGMENT_SECS: f32 = 30.0;

#[derive(Debug, PartialEq)]
enum VadState {
    Silence,
    Speech,
}

pub struct LiveConfig {
    pub vad_silence_ms: u64,
    pub energy_threshold: f32,
    pub input_device: Option<String>,
    pub output_path: Option<PathBuf>,
    pub lm_url: String,
    pub lm_model: String,
    pub no_llm: bool,
}

pub fn run_live(cfg: &LiveConfig, asr: &AsrEngine) -> Result<()> {
    let selection = crate::audio::select_input_device(cfg.input_device.as_deref())?;
    let device = selection.device;

    let stream_config = device.default_input_config()?;
    let sample_rate = stream_config.sample_rate().0;
    let channels = stream_config.channels() as usize;

    tracing::info!(
        "Mic: {} | {}Hz {}ch {:?}",
        selection.name,
        sample_rate,
        channels,
        stream_config.sample_format()
    );

    if cfg.no_llm {
        println!("Listening... (LLM disabled, press Ctrl+C to stop)");
    } else {
        println!(
            "Listening... (LLM polish via {}, press Ctrl+C to stop)",
            cfg.lm_url
        );
    }

    // ── CPAL audio channel ──────────────────────────────────────────────────
    let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<f32>>(64);

    // ── LLM worker channel: ASR text → LLM polish → clipboard ─────────────
    // The LLM call is blocking and can take 1-3s. Run it in a dedicated thread
    // so the mic capture loop is never blocked.
    let (llm_tx, llm_rx) = mpsc::channel::<String>();
    {
        let lm_url = cfg.lm_url.clone();
        let lm_model = cfg.lm_model.clone();
        let no_llm = cfg.no_llm;
        let output_path = cfg.output_path.clone();

        std::thread::spawn(move || {
            llm_worker(llm_rx, &lm_url, &lm_model, no_llm, output_path.as_ref());
        });
    }

    // ── Ctrl+C ──────────────────────────────────────────────────────────────
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || {
            println!("\nStopping...");
            running.store(false, Ordering::SeqCst);
        })
        .ok();
    }

    // ── CPAL stream ─────────────────────────────────────────────────────────
    let stream = build_f32_stream(&device, &stream_config.into(), audio_tx)?;
    stream.play()?;

    // ── VAD parameters ──────────────────────────────────────────────────────
    let silence_thresh_samples =
        (sample_rate as f64 * cfg.vad_silence_ms as f64 / 1000.0) as usize * channels;
    let max_seg_samples = (MAX_SEGMENT_SECS * sample_rate as f32) as usize * channels;
    let min_speech_samples =
        (MIN_SPEECH_SAMPLES as f64 * sample_rate as f64 / 16000.0) as usize * channels;

    let mut speech_buf: Vec<f32> = Vec::new();
    let mut silence_count: usize = 0;
    let mut state = VadState::Silence;

    // ── Main VAD loop ────────────────────────────────────────────────────────
    while running.load(Ordering::SeqCst) {
        let chunk = match audio_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let energy = rms_energy(&chunk, channels);

        match state {
            VadState::Silence => {
                if energy > cfg.energy_threshold {
                    state = VadState::Speech;
                    silence_count = 0;
                    speech_buf.clear();
                    speech_buf.extend_from_slice(&chunk);
                }
            }
            VadState::Speech => {
                speech_buf.extend_from_slice(&chunk);

                if energy < cfg.energy_threshold {
                    silence_count += chunk.len();
                    if silence_count >= silence_thresh_samples
                        && speech_buf.len() >= min_speech_samples
                    {
                        asr_and_enqueue(&speech_buf, sample_rate, channels as u16, asr, &llm_tx);
                        speech_buf.clear();
                        silence_count = 0;
                        state = VadState::Silence;
                    }
                } else {
                    silence_count = 0;
                    if speech_buf.len() >= max_seg_samples {
                        asr_and_enqueue(&speech_buf, sample_rate, channels as u16, asr, &llm_tx);
                        speech_buf.clear();
                        silence_count = 0;
                        state = VadState::Silence;
                    }
                }
            }
        }
    }

    // Flush remaining speech
    if state == VadState::Speech && speech_buf.len() >= min_speech_samples {
        asr_and_enqueue(&speech_buf, sample_rate, channels as u16, asr, &llm_tx);
    }

    // Drop llm_tx so the worker thread can exit cleanly
    drop(llm_tx);

    Ok(())
}

/// Run ASR on the segment and send raw text to the LLM worker channel.
fn asr_and_enqueue(
    raw: &[f32],
    sample_rate: u32,
    channels: u16,
    asr: &AsrEngine,
    tx: &mpsc::Sender<String>,
) {
    match to_mono_16k(raw, sample_rate, channels) {
        Ok(mono) => match asr.transcribe(&mono) {
            Ok(seg) if is_meaningful_pub(&seg.text) => {
                tracing::debug!("ASR → {}", seg.text);
                let _ = tx.send(seg.text);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("ASR error: {}", e),
        },
        Err(e) => tracing::warn!("Resample error: {}", e),
    }
}

/// Blocking LLM worker: receives ASR text, polishes with LMStudio, pastes to clipboard.
fn llm_worker(
    rx: mpsc::Receiver<String>,
    lm_url: &str,
    lm_model: &str,
    no_llm: bool,
    output_path: Option<&PathBuf>,
) {
    let mut writer: Option<std::io::BufWriter<std::fs::File>> = output_path.map(|p| {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .expect("Cannot open output file");
        std::io::BufWriter::new(f)
    });

    while let Ok(raw_text) = rx.recv() {
        let final_text = if no_llm {
            raw_text
        } else {
            match llm::polish_voice_blocking(lm_url, lm_model, &raw_text) {
                Ok(polished) => {
                    tracing::debug!("LLM → {}", polished);
                    polished
                }
                Err(e) => {
                    tracing::warn!("LLM polish failed (using raw): {}", e);
                    raw_text
                }
            }
        };

        println!("{}", final_text);
        set_clipboard_pub(&final_text);

        if let Some(ref mut w) = writer {
            use std::io::Write;
            let _ = writeln!(w, "{}", final_text);
            let _ = w.flush();
        }
    }
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

pub fn set_clipboard_pub(text: &str) {
    match arboard::Clipboard::new() {
        Ok(mut cb) => {
            if let Err(e) = cb.set_text(text) {
                tracing::warn!("Clipboard write failed: {}", e);
            }
        }
        Err(e) => tracing::warn!("Clipboard init failed: {}", e),
    }
}

fn build_f32_stream(
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

    Ok(stream)
}

/// Filter out segments that are only punctuation or too short.
pub fn is_meaningful_pub(text: &str) -> bool {
    let meaningful_chars = text
        .chars()
        .filter(|c| {
            c.is_alphanumeric()
                && !matches!(
                    *c,
                    '。' | '，' | '、' | '！' | '？' | '…' | '·' | '.' | ',' | '!' | '?'
                )
        })
        .count();
    meaningful_chars >= 2
}
