use anyhow::Result;
use std::fmt::Write as FmtWrite;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct TranscriptSegment {
    pub start_sec: f64,
    pub end_sec: f64,
    pub speaker: Option<String>,
    pub text: String,
}

/// Render transcription segments into a Markdown document and write to file.
///
/// `segments` — transcript segments, optionally with speaker labels.
/// `duration_secs` — total audio duration for the header.
pub fn write_markdown(
    segments: &[TranscriptSegment],
    source_name: &str,
    duration_secs: f64,
    output_path: &Path,
) -> Result<()> {
    let content = render(segments, source_name, duration_secs);
    std::fs::write(output_path, content)?;
    Ok(())
}

pub fn render(segments: &[TranscriptSegment], source_name: &str, duration_secs: f64) -> String {
    let mut doc = String::new();

    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let duration = fmt_duration(duration_secs as u64);

    let _ = writeln!(doc, "# 会议转写");
    let _ = writeln!(doc);
    let _ = writeln!(doc, "**来源**: {}  ", source_name);
    let _ = writeln!(doc, "**日期**: {}  ", now);
    let _ = writeln!(doc, "**时长**: {}  ", duration);
    let _ = writeln!(doc);
    let _ = writeln!(doc, "---");
    let _ = writeln!(doc);

    for seg in merge_adjacent_segments(segments) {
        if seg.text.is_empty() {
            continue;
        }
        let title = match &seg.speaker {
            Some(speaker) => format!(
                "## {} {} - {}",
                speaker,
                fmt_offset(seg.start_sec as u64),
                fmt_offset(seg.end_sec.max(seg.start_sec) as u64),
            ),
            None => format!(
                "## {} - {}",
                fmt_offset(seg.start_sec as u64),
                fmt_offset(seg.end_sec.max(seg.start_sec) as u64),
            ),
        };
        let _ = writeln!(doc, "{title}");
        let _ = writeln!(doc);
        let _ = writeln!(doc, "{}", seg.text.trim());
        let _ = writeln!(doc);
    }

    doc
}

fn merge_adjacent_segments(segments: &[TranscriptSegment]) -> Vec<TranscriptSegment> {
    let mut merged: Vec<TranscriptSegment> = Vec::new();

    for seg in segments.iter().filter(|s| !s.text.trim().is_empty()) {
        if let Some(last) = merged.last_mut() {
            let same_speaker = last.speaker == seg.speaker;
            let close_in_time = (seg.start_sec - last.end_sec).abs() <= 0.8;
            if same_speaker && close_in_time {
                if !last.text.ends_with('\n') {
                    last.text.push(' ');
                }
                last.text.push_str(seg.text.trim());
                last.end_sec = seg.end_sec.max(last.end_sec);
                continue;
            }
        }
        merged.push(seg.clone());
    }

    merged
}

fn fmt_offset(secs: u64) -> String {
    format!(
        "[{:02}:{:02}:{:02}]",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn fmt_duration(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}
