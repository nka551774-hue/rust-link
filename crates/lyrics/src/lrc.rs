//! The LRC format, which is what a timed lyrics provider answers with.
//!
//! A line carries one or more `[mm:ss.xx]` tags and the words that follow them.
//! A line with several tags is the same words at several times, which is how a
//! repeated chorus is written once.

use crate::Line;
use rustlink_protocol::Object;

// Repeated timestamps duplicate the following text. Bound the expanded result,
// not only the provider's response, before allocating those copies.
const MAX_LINES: usize = 10_000;
const MAX_TEXT_BYTES: usize = 8 << 20;

/// Turns the text a timed provider returns into lines.
///
/// A tag that names a field rather than a time, such as `[ar:...]`, carries no
/// lyrics and ends the scan for that line, so a header is skipped rather than
/// read as a line at time zero.
pub fn parse(text: &str) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut text_bytes = 0usize;
    for raw in text.split('\n') {
        let raw = raw.trim_end_matches('\r');
        let mut rest = raw;
        let mut stamps: Vec<i64> = Vec::new();
        while let Some((taken, at)) = leading_stamp(rest) {
            stamps.push(at);
            rest = &rest[taken..];
        }
        if stamps.is_empty() {
            continue;
        }
        let words = rest.trim().to_string();
        text_bytes = text_bytes.saturating_add(stamps.len().saturating_mul(words.len()));
        if lines.len().saturating_add(stamps.len()) > MAX_LINES || text_bytes > MAX_TEXT_BYTES {
            return Vec::new();
        }
        for at in stamps {
            lines.push(Line {
                timestamp: at,
                duration: None,
                line: words.clone(),
                plugin: Object::new(),
            });
        }
    }
    // Providers usually write in order, but a file with a tag out of place would
    // otherwise show a line before the one that comes first.
    lines.sort_by_key(|line| line.timestamp);
    fill_durations(&mut lines);
    lines
}

/// Reads one `[mm:ss.xx]` tag from the front of `text`, reporting how many bytes
/// it took and the time it names in milliseconds.
///
/// The fraction may be one, two or three digits, meaning tenths, hundredths or
/// milliseconds.
fn leading_stamp(text: &str) -> Option<(usize, i64)> {
    if !text.starts_with('[') {
        return None;
    }
    let end = text.find(']')?;
    let body = &text[1..end];
    let colon = body.find(':')?;
    let decimal = |digits: &str| -> Option<i64> {
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    };
    let minutes = decimal(body[..colon].trim())?;
    let mut seconds = body[colon + 1..].trim();
    let mut fraction = 0i64;
    if let Some(dot) = seconds.find('.') {
        let digits = &seconds[dot + 1..];
        seconds = &seconds[..dot];
        if digits.len() > 3 {
            return None;
        }
        let value = decimal(digits)?;
        fraction = match digits.len() {
            1 => value * 100,
            2 => value * 10,
            _ => value,
        };
    }
    let whole = decimal(seconds)?;
    if whole >= 60 {
        return None;
    }
    let timestamp =
        minutes.checked_mul(60_000)?.checked_add(whole * 1000)?.checked_add(fraction)?;
    Some((end + 1, timestamp))
}

/// Gives every line but the last the gap to the next one, which is how long it
/// stays on screen. The provider does not say, and the gap is what a line by
/// line display wants.
fn fill_durations(lines: &mut [Line]) {
    for index in 0..lines.len().saturating_sub(1) {
        let gap = (lines[index + 1].timestamp - lines[index].timestamp).max(0);
        lines[index].duration = Some(gap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_line_carries_its_time_and_words() {
        let lines = parse("[00:12.34]hello");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].timestamp, 12_340);
        assert_eq!(lines[0].line, "hello");
    }

    #[test]
    fn a_line_with_several_tags_becomes_several_lines() {
        let lines = parse("[00:01.00][00:02.00]again");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].timestamp, 1_000);
        assert_eq!(lines[1].timestamp, 2_000);
        assert_eq!(lines[0].line, "again");
        assert_eq!(lines[1].line, "again");
    }

    #[test]
    fn the_fraction_reads_as_tenths_hundredths_or_milliseconds() {
        assert_eq!(parse("[00:00.5]a")[0].timestamp, 500);
        assert_eq!(parse("[00:00.50]a")[0].timestamp, 500);
        assert_eq!(parse("[00:00.500]a")[0].timestamp, 500);
    }

    #[test]
    fn a_header_tag_is_not_a_line() {
        assert!(parse("[ar:someone]\n[00:01.00]words").len() == 1);
    }

    #[test]
    fn a_line_out_of_order_is_sorted() {
        let lines = parse("[00:05.00]second\n[00:01.00]first");
        assert_eq!(lines[0].line, "first");
        assert_eq!(lines[1].line, "second");
    }

    #[test]
    fn every_line_but_the_last_knows_how_long_it_lasts() {
        let lines = parse("[00:01.00]a\n[00:03.00]b");
        assert_eq!(lines[0].duration, Some(2_000));
        assert_eq!(lines[1].duration, None);
    }

    #[test]
    fn text_with_no_tags_is_no_lyrics() {
        assert!(parse("just words\nand more words").is_empty());
    }

    #[test]
    fn malformed_and_overflowing_timestamps_are_skipped() {
        for stamp in [
            "[-1:02]",
            "[00:-1]",
            "[00:60]",
            "[00:00.-1]",
            "[+1:02]",
            "[00:00.]",
            "[9223372036854775807:00]",
            "[153722867280912:59.999]",
        ] {
            let lines = parse(&format!("{stamp}invalid\n[00:01.00]valid"));
            assert_eq!(lines.len(), 1, "{stamp}");
            assert_eq!(lines[0].timestamp, 1000, "{stamp}");
            assert_eq!(lines[0].line, "valid");
        }
        assert_eq!(parse("[153722867280912:55.807]last")[0].timestamp, i64::MAX);
    }

    #[test]
    fn repeated_timestamps_cannot_expand_an_answer_without_a_bound() {
        let large_line = format!("{}{}", "[00:01]".repeat(20), "a".repeat(512 << 10));
        assert!(large_line.len() < 1 << 20);
        assert!(parse(&large_line).is_empty());
        assert!(parse(&format!("{}words", "[00:01]".repeat(MAX_LINES + 1))).is_empty());
    }
}
