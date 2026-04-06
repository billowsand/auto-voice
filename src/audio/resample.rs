use anyhow::Result;
use rubato::{FftFixedIn, Resampler};

/// Convert interleaved multi-channel audio to mono 16kHz f32 samples.
pub fn to_mono_16k(samples: &[f32], src_rate: u32, channels: u16) -> Result<Vec<f32>> {
    // Step 1: de-interleave and mix to mono
    let mono = if channels == 1 {
        samples.to_vec()
    } else {
        let ch = channels as usize;
        let frames = samples.len() / ch;
        let mut mono = Vec::with_capacity(frames);
        for i in 0..frames {
            let sum: f32 = (0..ch).map(|c| samples[i * ch + c]).sum();
            mono.push(sum / ch as f32);
        }
        mono
    };

    // Step 2: resample to 16kHz if needed
    if src_rate == 16000 {
        return Ok(mono);
    }

    let target_rate: u32 = 16000;
    let chunk_size = 1024usize;

    let mut resampler = FftFixedIn::<f32>::new(
        src_rate as usize,
        target_rate as usize,
        chunk_size,
        2, // sub_chunks
        1, // channels (already mono)
    )?;

    let mut output = Vec::new();
    let mut pos = 0usize;

    while pos < mono.len() {
        let end = (pos + chunk_size).min(mono.len());
        let mut chunk = mono[pos..end].to_vec();
        // Pad last chunk if needed
        chunk.resize(chunk_size, 0.0);
        pos = end;

        let resampled = resampler.process(&[chunk], None)?;
        output.extend_from_slice(&resampled[0]);
    }

    // Flush any remaining samples
    let flushed = resampler.process_partial::<Vec<f32>>(None, None)?;
    if let Some(ch) = flushed.first() {
        output.extend_from_slice(ch);
    }

    Ok(output)
}
