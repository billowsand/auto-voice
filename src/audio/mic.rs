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
        if let Err(error) = set_clipboard_pub(&final_text) {
            tracing::warn!("Clipboard write failed: {error:#}");
        }

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

pub fn set_clipboard_pub(text: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    if crate::platform::is_wayland_session() {
        use anyhow::Context;
        use std::io::Write;

        let mut child = std::process::Command::new("wl-copy")
            .args(["--type", "text/plain;charset=utf-8"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("failed to start wl-copy")?;
        child
            .stdin
            .take()
            .context("wl-copy stdin is unavailable")?
            .write_all(text.as_bytes())
            .context("failed to send text to wl-copy")?;
        // wl-copy forks a daemon that serves the selection until another client replaces it,
        // and the daemon inherits our stderr pipe. wait_with_output() reads stderr to EOF
        // before reaping, which would block for as long as our text stays on the clipboard —
        // the dictation cycle hangs at "正在转写" and the paste is never sent. Reap the
        // foreground process as soon as it exits instead, and only read stderr when it
        // failed: on failure no daemon was forked, so the pipe reaches EOF on its own.
        let status = child.wait().context("failed to wait for wl-copy")?;
        if !status.success() {
            use std::io::Read;
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            anyhow::bail!("wl-copy failed: {}", stderr.trim());
        }
        tracing::info!("Clipboard ready through native Wayland wl-copy");
        return Ok(());
    }

    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text)?;
    Ok(())
}

/// Give the clipboard a moment to actually be ready to serve `expected` before a synthetic paste
/// is dispatched.
///
/// On Wayland, `wl-copy` (see [`set_clipboard_pub`]) forks a background daemon that answers
/// `wl_data_device` selection requests; a fixed delay was previously used to guess when it was
/// up, and a slow start could race the target application's own request, which then saw stale or
/// empty clipboard content — a paste that silently inserted nothing. This instead round-trips
/// through `wl-paste`, the same protocol path a real paste would take, and returns as soon as it
/// reads back what was just set. Falls back to a fixed wait when `wl-paste` is missing or the
/// session is not Wayland, where clipboard writes are effectively synchronous.
pub fn wait_clipboard_ready_pub(expected: &str) {
    #[cfg(target_os = "linux")]
    if crate::platform::is_wayland_session() {
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        loop {
            match std::process::Command::new("wl-paste")
                .args(["--no-newline", "--type", "text/plain;charset=utf-8"])
                .output()
            {
                Ok(output)
                    if output.status.success()
                        && String::from_utf8_lossy(&output.stdout) == expected =>
                {
                    return;
                }
                // wl-paste isn't installed: nothing to poll for, so give the daemon a fixed
                // head start instead of busy-spawning a command that can never succeed.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    std::thread::sleep(Duration::from_millis(120));
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
