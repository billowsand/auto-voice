//! Live transcript preview: what the recogniser has heard so far, while it is still being said.
//!
//! The models behind auto-voice are offline recognisers — they want a whole utterance and give
//! back a whole transcript — so there is no partial-result callback to subscribe to. Instead a
//! background worker re-runs the recogniser over the audio captured so far a couple of times a
//! second and pushes the result onto the overlay. The dictation loop is never blocked by it:
//! [`LivePreview::push`] only appends to a buffer, and nothing here can change what eventually
//! gets inserted, which is still decided by one final pass over the complete recording.
//!
//! Re-reading the whole buffer every pass would get quadratically slower the longer someone
//! talks, so once a stretch is long enough the worker commits it: the text up to a pause is
//! frozen into a prefix and only the audio after it is re-read from then on.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::asr::AsrEngine;
use crate::audio::resample::to_mono_16k;
use crate::osd::OsdHandle;

/// Minimum gap between two preview passes. Fast enough to feel live, slow enough that the
/// recogniser is idle most of the time.
const PASS_INTERVAL: Duration = Duration::from_millis(650);
/// Ceiling for the backoff a slow model earns itself.
const MAX_INTERVAL: Duration = Duration::from_secs(3);
/// Below this there is not enough speech for a useful guess.
const MIN_AUDIO_SECS: f32 = 0.9;
/// A tail longer than this is frozen at the next pause.
const COMMIT_AFTER_SECS: f32 = 8.0;
/// 20ms at 16kHz: the resolution the pause search works at.
const FRAME: usize = 320;

struct Shared {
    raw: Mutex<Vec<f32>>,
    grew: Condvar,
    stopped: AtomicBool,
}

/// Handle held by the dictation loop for the duration of one utterance. Dropping it retires
/// the worker; a pass already under way finishes and is discarded by the overlay.
pub struct LivePreview {
    shared: Arc<Shared>,
}

impl LivePreview {
    pub fn start(
        engine: Arc<AsrEngine>,
        osd: OsdHandle,
        sample_rate: u32,
        channels: u16,
    ) -> Option<Self> {
        let shared = Arc::new(Shared {
            raw: Mutex::new(Vec::new()),
            grew: Condvar::new(),
            stopped: AtomicBool::new(false),
        });
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("asr-preview".to_owned())
            .spawn(move || run(worker, engine, osd, sample_rate, channels))
            .map_err(|error| tracing::warn!("Live preview unavailable: {error}"))
            .ok()?;
        Some(Self { shared })
    }

    /// Hand over one capture buffer, exactly as it came off the microphone.
    pub fn push(&self, chunk: &[f32]) {
        let mut raw = lock(&self.shared.raw);
        raw.extend_from_slice(chunk);
        drop(raw);
        self.shared.grew.notify_one();
    }
}

impl Drop for LivePreview {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.grew.notify_all();
    }
}

fn run(
    shared: Arc<Shared>,
    engine: Arc<AsrEngine>,
    osd: OsdHandle,
    sample_rate: u32,
    channels: u16,
) {
    let stride = channels.max(1) as usize;
    let mut committed_text = String::new();
    // Grows if a pass turns out to be expensive. FunASR-nano runs a language model per pass and
    // would otherwise queue passes back to back and starve the final transcription of CPU.
    let mut interval = PASS_INTERVAL;
    let mut last_pass = Instant::now() - interval;

    while let Some(raw) = wait_for_audio(&shared, sample_rate, stride, interval, &mut last_pass) {
        let started = Instant::now();
        let Ok(mono) = to_mono_16k(&raw, sample_rate, channels) else {
            continue;
        };
        if mono.is_empty() {
            continue;
        }

        // Decoding runs against the same recogniser the dictation loop uses. That is sound —
        // `OfflineRecognizer` is `Sync` and every pass gets its own stream — and it means a
        // preview still in flight never delays the text the user is actually waiting for.
        let Ok(tail) = engine.transcribe(&mono) else {
            continue;
        };
        if shared.stopped.load(Ordering::Acquire) {
            return;
        }
        osd.set_partial(&joined(&committed_text, &tail.text));
        interval = PASS_INTERVAL
            .max(started.elapsed().mul_f32(1.5))
            .min(MAX_INTERVAL);

        // Long enough to be worth freezing? Cut at the quietest moment in the second half, so
        // the boundary falls between words rather than through one, and drop the frozen audio:
        // from here on only what came after the pause is read again.
        if mono.len() as f32 / 16_000.0 >= COMMIT_AFTER_SECS {
            if let Some(split) = quiet_split(&mono) {
                if let Ok(head) = engine.transcribe(&mono[..split]) {
                    committed_text = joined(&committed_text, &head.text);
                    let mut buffer = lock(&shared.raw);
                    let frozen = raw_offset(split, sample_rate, stride).min(buffer.len());
                    buffer.drain(..frozen);
                }
            }
        }
    }
}

/// Block until there is enough audio for another pass. `None` means the utterance is over.
fn wait_for_audio(
    shared: &Shared,
    sample_rate: u32,
    stride: usize,
    interval: Duration,
    last_pass: &mut Instant,
) -> Option<Vec<f32>> {
    let mut raw = lock(&shared.raw);
    loop {
        if shared.stopped.load(Ordering::Acquire) {
            return None;
        }
        let waited = last_pass.elapsed();
        if waited >= interval && seconds(raw.len(), sample_rate, stride) >= MIN_AUDIO_SECS {
            *last_pass = Instant::now();
            return Some(raw.clone());
        }
        let timeout = interval
            .saturating_sub(waited)
            .max(Duration::from_millis(30));
        raw = shared
            .grew
            .wait_timeout(raw, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn seconds(samples: usize, sample_rate: u32, stride: usize) -> f32 {
    if sample_rate == 0 || stride == 0 {
        return 0.0;
    }
    samples as f32 / stride as f32 / sample_rate as f32
}

/// Where a 16kHz mono offset lands in the interleaved capture buffer, rounded to a frame.
fn raw_offset(mono_index: usize, sample_rate: u32, stride: usize) -> usize {
    let frames = (mono_index as f64 * sample_rate as f64 / 16_000.0) as usize;
    frames * stride
}

/// Glue a frozen prefix to the newest guess. Latin words need the space that Chinese does not.
fn joined(head: &str, tail: &str) -> String {
    let (head, tail) = (head.trim_end(), tail.trim());
    if head.is_empty() {
        return tail.to_owned();
    }
    if tail.is_empty() {
        return head.to_owned();
    }
    let spaced = head.ends_with(|c: char| c.is_ascii_alphanumeric())
        && tail.starts_with(|c: char| c.is_ascii_alphanumeric());
    if spaced {
        format!("{head} {tail}")
    } else {
        format!("{head}{tail}")
    }
}

/// The quietest 20ms in the middle-to-late part of the buffer, if it is quiet enough to be a
/// pause rather than just a soft syllable. Returned as a sample offset.
fn quiet_split(mono: &[f32]) -> Option<usize> {
    let frames = mono.len() / FRAME;
    if frames < 25 {
        return None;
    }
    let energy = |frame: usize| {
        let window = &mono[frame * FRAME..(frame + 1) * FRAME];
        (window.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt()
    };

    let average = (0..frames).map(energy).sum::<f32>() / frames as f32;
    let (quietest, level) = (frames * 45 / 100..frames * 90 / 100)
        .map(|frame| (frame, energy(frame)))
        .min_by(|left, right| left.1.total_cmp(&right.1))?;

    // Cutting through speech would garble the word on both sides of the seam.
    (level <= average * 0.35).then_some(quietest * FRAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frozen_prefix_is_glued_to_the_newest_guess() {
        assert_eq!(joined("", "你好"), "你好");
        assert_eq!(joined("今天天气", "不错"), "今天天气不错");
        assert_eq!(joined("hello", "world"), "hello world");
        assert_eq!(joined("你好，", "world"), "你好，world");
        assert_eq!(joined("今天天气", ""), "今天天气");
    }

    #[test]
    fn a_pause_is_found_between_two_bursts_of_speech() {
        // Loud, silent, loud: the seam belongs in the silence in the middle.
        let mut mono = vec![0.0f32; FRAME * 60];
        for (index, sample) in mono.iter_mut().enumerate() {
            let frame = index / FRAME;
            *sample = if (24..34).contains(&frame) { 0.0 } else { 0.4 };
        }
        let split = quiet_split(&mono).expect("a pause in the second half");
        assert!(
            (24 * FRAME..34 * FRAME).contains(&split),
            "split {split} should land in the silence"
        );
    }

    #[test]
    fn continuous_speech_is_never_cut() {
        let mono: Vec<f32> = (0..FRAME * 60)
            .map(|index| (index as f32 * 0.05).sin() * 0.4)
            .collect();
        assert_eq!(quiet_split(&mono), None);
    }

    #[test]
    fn short_buffers_are_left_alone() {
        assert_eq!(quiet_split(&vec![0.0; FRAME * 10]), None);
    }

    #[test]
    fn mono_offsets_map_back_onto_interleaved_frames() {
        // One second of 16kHz mono is one second of 48kHz stereo.
        assert_eq!(raw_offset(16_000, 48_000, 2), 48_000 * 2);
        assert_eq!(raw_offset(0, 44_100, 1), 0);
    }
}
