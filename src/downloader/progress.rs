/// A single parsed progress snapshot from yt-dlp's `--progress-template` output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProgressState {
    /// 0.0 ..= 100.0 for the current file/stream (resets between video & audio).
    pub percent: f32,
    pub speed: String,
    pub eta: String,
    /// Pre-formatted byte counts from yt-dlp (e.g. "12.34MiB"), or "—".
    pub downloaded: String,
    pub total: String,
    /// Fragment counters for HLS/DASH streams, when present.
    pub frag_index: Option<u32>,
    pub frag_count: Option<u32>,
}

/// Which stage of the job the user is currently watching. yt-dlp announces the
/// post-processing stages on stdout; [`crate::downloader::detect_phase`] maps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Preparing,
    Downloading,
    Merging,
    Converting,
    Finalizing,
}

/// Events streamed from the download task to the UI task. The UI keeps only the
/// latest of each and renders on its own heartbeat, so dropped frames are fine.
#[derive(Clone, Debug)]
pub enum ProgressEvent {
    Update(ProgressState),
    Phase(Phase),
}

/// Braille spinner frames; advanced once per UI edit so the message visibly
/// "breathes" even while a stage emits no new numbers (merging, converting).
pub const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Sentinel that marks a parseable progress line. It must be re-emitted *inside*
/// the template body: yt-dlp consumes the text before the first `:` of
/// `--progress-template` as the template *type* (`download`) and strips it, so a
/// bare `download:` prefix would never reach our output (this was the original
/// bug — progress lines silently never matched).
pub const PROGRESS_PREFIX: &str = "PROGRESS:";

/// yt-dlp progress template, pipe-delimited. The leading `download:` selects the
/// download-progress type (stripped by yt-dlp); [`PROGRESS_PREFIX`] is what
/// actually prefixes each emitted line. Field order must match
/// [`parse_progress_line`].
pub const PROGRESS_TEMPLATE: &str = concat!(
    "download:",
    "PROGRESS:",
    "%(progress._percent_str)s|",
    "%(progress._speed_str)s|",
    "%(progress._eta_str)s|",
    "%(progress._downloaded_bytes_str)s|",
    "%(progress._total_bytes_str)s|",
    "%(progress._total_bytes_estimate_str)s|",
    "%(progress.fragment_index)s|",
    "%(progress.fragment_count)s",
);

/// Parse one line emitted by [`PROGRESS_TEMPLATE`].
/// Returns `None` for non-progress lines or an unparseable percentage.
pub fn parse_progress_line(line: &str) -> Option<ProgressState> {
    let rest = line.strip_prefix(PROGRESS_PREFIX)?;
    let mut f = rest.split('|');

    let percent = parse_percent(f.next()?)?;
    let speed = clean(f.next().unwrap_or(""));
    let eta = clean(f.next().unwrap_or(""));
    let downloaded = clean(f.next().unwrap_or(""));
    let total = clean_total(f.next().unwrap_or(""), f.next().unwrap_or(""));
    let frag_index = parse_u32(f.next().unwrap_or(""));
    let frag_count = parse_u32(f.next().unwrap_or(""));

    Some(ProgressState {
        percent,
        speed,
        eta,
        downloaded,
        total,
        frag_index,
        frag_count,
    })
}

fn parse_percent(s: &str) -> Option<f32> {
    let v = s.trim().trim_end_matches('%').trim().parse::<f32>().ok()?;
    // `clamp` propagates NaN, which would render as "nan%" — reject it instead.
    if v.is_nan() {
        return None;
    }
    Some(v.clamp(0.0, 100.0))
}

/// Minimal HTML escaping for dynamic fields embedded in a `parse_mode=Html`
/// message. yt-dlp's byte/speed strings are normally plain, but escape anyway.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn parse_u32(s: &str) -> Option<u32> {
    s.trim().parse::<u32>().ok()
}

/// Normalise yt-dlp's many "I don't know" spellings to a single em dash.
fn clean(s: &str) -> String {
    let t = s.trim();
    let lo = t.to_ascii_lowercase();
    if t.is_empty() || lo == "n/a" || lo == "na" || lo == "none" || lo.starts_with("unknown") {
        "—".to_string()
    } else {
        t.to_string()
    }
}

/// Prefer the exact total; fall back to the (prefixed) estimate; else em dash.
fn clean_total(total: &str, estimate: &str) -> String {
    let t = clean(total);
    if t != "—" {
        return t;
    }
    let e = clean(estimate);
    if e != "—" {
        // Strip any leading "~" so a tilde-prefixed estimate can't become "~~".
        return format!("~{}", e.trim_start_matches('~'));
    }
    "—".to_string()
}

/// Render a fixed-width bar from filled (`▰`) and empty (`▱`) segments. Shared by
/// the download and upload bars so they look identical.
pub fn bar(percent: f32, width: usize) -> String {
    let ratio = (percent / 100.0).clamp(0.0, 1.0);
    let filled = ((ratio * width as f32).round() as usize).min(width);
    format!("{}{}", "▰".repeat(filled), "▱".repeat(width - filled))
}

/// Render the single-line progress message for the current `phase`, e.g.
/// `⬇️ Downloading: ▰▰▰▰▱▱▱▱▱▱ 42% (12.3MiB / 29.0MiB) · 2.3MiB/s · ETA 00:08`.
/// Indeterminate stages (merge/convert) show an animated `spinner` instead.
pub fn render(phase: Phase, p: &ProgressState, spinner: char, width: usize) -> String {
    if phase != Phase::Downloading {
        let (emoji, label) = phase_label(phase);
        return format!("{emoji} {label} {spinner}");
    }

    let pct = p.percent.clamp(0.0, 100.0);
    let mut s = format!("⬇️ Downloading: {} {pct:.0}%", bar(pct, width));

    // Size in parentheses: "done / total" when both known, else whichever we have.
    match (p.downloaded.as_str(), p.total.as_str()) {
        ("—", "—") => {}
        ("—", t) => s.push_str(&format!(" ({})", esc(t))),
        (d, "—") => s.push_str(&format!(" ({})", esc(d))),
        (d, t) => s.push_str(&format!(" ({} / {})", esc(d), esc(t))),
    }

    let mut extra: Vec<String> = Vec::new();
    if p.speed != "—" {
        extra.push(esc(&p.speed));
    }
    if p.eta != "—" {
        extra.push(format!("ETA {}", esc(&p.eta)));
    }
    if let (Some(i), Some(n)) = (p.frag_index, p.frag_count) {
        extra.push(format!("frag {i}/{n}"));
    }
    if !extra.is_empty() {
        s.push_str(" · ");
        s.push_str(&extra.join(" · "));
    }
    s
}

fn phase_label(phase: Phase) -> (&'static str, &'static str) {
    match phase {
        Phase::Preparing => ("⏳", "Starting…"),
        Phase::Downloading => ("⬇️", "Downloading…"),
        Phase::Merging => ("🧩", "Merging audio + video…"),
        Phase::Converting => ("🎵", "Converting to MP3…"),
        Phase::Finalizing => ("✨", "Finalizing…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dl(percent: f32) -> ProgressState {
        ProgressState {
            percent,
            speed: "—".into(),
            eta: "—".into(),
            downloaded: "—".into(),
            total: "—".into(),
            frag_index: None,
            frag_count: None,
        }
    }

    #[test]
    fn parses_full_line() {
        let p = parse_progress_line(
            "PROGRESS:  45.2%|2.34MiB/s|00:12|12.34MiB|27.30MiB|NA|3|120",
        )
        .unwrap();
        assert!((p.percent - 45.2).abs() < 0.01);
        assert_eq!(p.speed, "2.34MiB/s");
        assert_eq!(p.eta, "00:12");
        assert_eq!(p.downloaded, "12.34MiB");
        assert_eq!(p.total, "27.30MiB");
        assert_eq!(p.frag_index, Some(3));
        assert_eq!(p.frag_count, Some(120));
    }

    #[test]
    fn handles_unknowns_and_estimate_fallback() {
        // Total unknown but an estimate is present → prefixed with "~".
        let p = parse_progress_line(
            "PROGRESS:100.0%|Unknown B/s|N/A|10.0MiB|NA|9.50MiB|NA|NA",
        )
        .unwrap();
        assert_eq!(p.percent, 100.0);
        assert_eq!(p.speed, "—");
        assert_eq!(p.eta, "—");
        assert_eq!(p.total, "~9.50MiB");
        assert_eq!(p.frag_index, None);
        assert_eq!(p.frag_count, None);
    }

    #[test]
    fn rejects_non_progress_and_bad_percent() {
        assert!(parse_progress_line("[download] Destination: foo.mp4").is_none());
        // A bare yt-dlp progress line (no sentinel) must not match.
        assert!(parse_progress_line("  45.2%|2.34MiB/s|00:12|x|y|z|u|t").is_none());
        assert!(parse_progress_line("PROGRESS:N/A%|x|y|z|w|v|u|t").is_none());
        // NaN must be rejected, not rendered as "nan%".
        assert!(parse_progress_line("PROGRESS:NaN%|x|y|z|w|v|u|t").is_none());
    }

    #[test]
    fn stats_line_never_shows_dangling_dash() {
        let mut p = dl(10.0);
        p.downloaded = "5.0MiB".into(); // total still unknown
        let s = render(Phase::Downloading, &p, '⠋', 12);
        assert!(s.contains("(5.0MiB)"));
        assert!(!s.contains("/ —"));
        assert!(!s.contains("— /"));
    }

    #[test]
    fn escapes_html_in_dynamic_fields() {
        let mut p = dl(50.0);
        p.speed = "<b>x</b>&y/s".into();
        let s = render(Phase::Downloading, &p, '⠋', 12);
        assert!(s.contains("&lt;b&gt;x&lt;/b&gt;&amp;y/s"));
        assert!(!s.contains("<b>x</b>"));
    }

    #[test]
    fn bar_uses_segments_and_keeps_width() {
        assert_eq!(bar(0.0, 10), "▱".repeat(10));
        assert_eq!(bar(100.0, 10), "▰".repeat(10));
        assert_eq!(bar(50.0, 10), format!("{}{}", "▰".repeat(5), "▱".repeat(5)));
        // A bar should never exceed its width in visible cells.
        for pct in [0.0, 1.0, 33.3, 66.6, 99.9, 100.0] {
            assert_eq!(bar(pct, 10).chars().count(), 10);
        }
    }

    #[test]
    fn renders_download_and_postprocess_stages() {
        let mut p = dl(40.0);
        p.downloaded = "12.3MiB".into();
        p.total = "29.0MiB".into();
        p.speed = "3.1MiB/s".into();
        p.eta = "00:05".into();
        let d = render(Phase::Downloading, &p, '⠹', 10);
        assert!(d.starts_with("⬇️ Downloading: "));
        assert!(d.contains("40%"));
        assert_eq!(d.matches('▰').count(), 4);
        assert!(d.contains("(12.3MiB / 29.0MiB)"));
        assert!(d.contains("3.1MiB/s"));
        assert!(d.contains("ETA 00:05"));

        // Indeterminate stages drop the bar and show a spinning label.
        let m = render(Phase::Merging, &p, '⠼', 10);
        assert!(m.contains("Merging audio + video… ⠼"));
        assert!(!m.contains('▰'));
        assert!(!m.contains('▱'));
    }
}
