use anyhow::{Context, Result};
use std::path::Path;
use symphonia::core::audio::AudioBufferRef;
use symphonia::core::audio::Signal;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub struct DecodedAudio {
    /// PCM samples, interleaved if multi-channel
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Decode any audio file supported by symphonia to f32 PCM.
pub fn decode_file(path: &Path) -> Result<DecodedAudio> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Cannot open audio file: {}", path.display()))?;

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("Unsupported audio format")?;

    let mut format = probed.format;

    // Pick the first audio track
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .context("No audio track found")?;

    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count() as u16)
        .unwrap_or(1);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("Failed to create decoder")?;

    let mut samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(symphonia::core::errors::Error::ResetRequired) => continue,
            Err(e) => return Err(e).context("Error reading packet"),
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                append_samples(&decoded, &mut samples);
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(e).context("Decode error"),
        }
    }

    Ok(DecodedAudio {
        samples,
        sample_rate,
        channels,
    })
}

fn append_samples(buf: &AudioBufferRef<'_>, out: &mut Vec<f32>) {
    use symphonia::core::audio::AudioBufferRef::*;
    match buf {
        F32(b) => append_planar_interleaved(b.planes().planes(), b.frames(), out, |s| *s),
        F64(b) => append_planar_interleaved(b.planes().planes(), b.frames(), out, |s| *s as f32),
        S32(b) => append_planar_interleaved(b.planes().planes(), b.frames(), out, |s| {
            *s as f32 / i32::MAX as f32
        }),
        S16(b) => append_planar_interleaved(b.planes().planes(), b.frames(), out, |s| {
            *s as f32 / i16::MAX as f32
        }),
        U8(b) => append_planar_interleaved(b.planes().planes(), b.frames(), out, |s| {
            (*s as f32 - 128.0) / 128.0
        }),
        _ => {
            // Other formats: convert via f32 if possible
        }
    }
}

fn append_planar_interleaved<T, F>(planes: &[&[T]], frames: usize, out: &mut Vec<f32>, convert: F)
where
    F: Fn(&T) -> f32,
{
    if planes.is_empty() {
        return;
    }

    out.reserve(frames * planes.len());
    for frame in 0..frames {
        for plane in planes {
            if let Some(sample) = plane.get(frame) {
                out.push(convert(sample));
            }
        }
    }
}
