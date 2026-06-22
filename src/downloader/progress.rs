/// A single parsed progress snapshot from yt-dlp's `--progress-template` output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProgressState {
    /// 0.0 ..= 100.0
    pub percent: f32,
    pub speed: String,
    pub eta: String,
}

/// Parse one line emitted by our progress template:
/// `download:<percent>|<speed>|<eta>` (yt-dlp pads percent with spaces and a `%`).
/// Returns `None` for non-progress lines or unparseable percentages.
pub fn parse_progress_line(line: &str) -> Option<ProgressState> {
    let rest = line.strip_prefix("download:")?;
    let mut parts = rest.split('|');

    let percent_raw = parts.next()?.trim();
    let speed = parts.next().unwrap_or("").trim();
    let eta = parts.next().unwrap_or("").trim();

    let percent = percent_raw.trim_end_matches('%').trim().parse::<f32>().ok()?;

    Some(ProgressState {
        percent: percent.clamp(0.0, 100.0),
        speed: clean(speed),
        eta: clean(eta),
    })
}

fn clean(s: &str) -> String {
    if s.is_empty() || s.eq_ignore_ascii_case("n/a") {
        "—".to_string()
    } else {
        s.to_string()
    }
}

/// Render an HTML-formatted progress message with an ASCII bar.
/// `title` must already be HTML-escaped by the caller.
pub fn render_bar(title_escaped: &str, p: &ProgressState, width: usize) -> String {
    let pct = p.percent.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * width as f32).round() as usize;
    let filled = filled.min(width);
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(width - filled));
    format!(
        "⬇️ <b>{title_escaped}</b>\n<code>[{bar}]</code> {pct:.0}% · {speed} · ETA {eta}",
        speed = p.speed,
        eta = p.eta,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_line() {
        let p = parse_progress_line("download:  45.2%|2.34MiB/s|00:12").unwrap();
        assert!((p.percent - 45.2).abs() < 0.01);
        assert_eq!(p.speed, "2.34MiB/s");
        assert_eq!(p.eta, "00:12");
    }

    #[test]
    fn handles_na_fields() {
        let p = parse_progress_line("download:100.0%|N/A|N/A").unwrap();
        assert_eq!(p.percent, 100.0);
        assert_eq!(p.speed, "—");
        assert_eq!(p.eta, "—");
    }

    #[test]
    fn rejects_non_progress_and_bad_percent() {
        assert!(parse_progress_line("[download] Destination: foo.mp4").is_none());
        assert!(parse_progress_line("download:N/A%|x|y").is_none());
    }

    #[test]
    fn bar_boundaries() {
        let mk = |pct| ProgressState { percent: pct, speed: "—".into(), eta: "—".into() };
        assert!(render_bar("t", &mk(0.0), 12).contains("[░░░░░░░░░░░░]"));
        assert!(render_bar("t", &mk(100.0), 12).contains("[████████████]"));
        assert!(render_bar("t", &mk(50.0), 12).contains("██████░░░░░░"));
    }
}
