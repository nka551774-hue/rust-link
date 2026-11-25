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

/// Fast zero-allocation check for whether `text` contains at least one valid
/// LRC timestamped line within resource bounds.
pub(crate) fn has_lines(text: &str) -> bool {
    let mut found = false;
    let ok = scan_lrc(text, |_, _, _| {
        found = true;
    });
    ok && found
}

/// Turns the text a timed provider returns into lines.
///
/// A tag that names a field rather than a time, such as `[ar:...]`, carries no
/// lyrics and ends the scan for that line, so a header is skipped rather than
/// read as a line at time zero.
pub fn parse(text: &str) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut stamps: Vec<i64> = Vec::new();
    let ok = scan_lrc(text, |raw, words_offset, count| {
        stamps.clear();
        let mut rest = raw;
        for _ in 0..count {
            if let Some((taken, at)) = leading_stamp(rest) {
                stamps.push(at);
                rest = rest[taken..].trim_start_matches([' ', '\t']);
            }
        }
        let cleaned = strip_inline_stamps(raw[words_offset..].trim());
        for &at in &stamps {
            lines.push(Line {
                timestamp: at,
                duration: None,
                line: cleaned.as_ref().to_owned(),
                plugin: Object::new(),
            });
        }
    });
    if !ok {
        return Vec::new();
    }
    // Providers usually write in order, but a file with a tag out of place would
    // otherwise show a line before the one that comes first.
    lines.sort_by_key(|line| line.timestamp);
    fill_durations(&mut lines);
    lines
}

fn scan_lrc(text: &str, mut on_line: impl FnMut(&str, usize, usize)) -> bool {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut line_count = 0usize;
    let mut text_bytes = 0usize;
    for raw in text.split('\n') {
        let trimmed = raw.trim_end_matches('\r').trim_start_matches([' ', '\t']);
        let mut rest = trimmed;
        let mut stamps = 0usize;
        while let Some((taken, _)) = leading_stamp(rest) {
            stamps += 1;
            rest = rest[taken..].trim_start_matches([' ', '\t']);
        }
        if stamps == 0 {
            continue;
        }
        let words_offset = trimmed.len() - rest.len();
        let words_len = rest.trim().len();
        line_count = line_count.saturating_add(stamps);
        text_bytes = text_bytes.saturating_add(stamps.saturating_mul(words_len));
        if line_count > MAX_LINES || text_bytes > MAX_TEXT_BYTES {
            return false;
        }
        on_line(trimmed, words_offset, stamps);
    }
    true
}

fn strip_inline_stamps(words: &str) -> std::borrow::Cow<'_, str> {
    if !words.contains('<') {
        return std::borrow::Cow::Borrowed(words);
    }
    let mut out = String::with_capacity(words.len());
    let mut rest = words;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let candidate = &rest[open..];
        if let Some(end) = candidate.find('>') {
            let bracketed = format!("[{}]", &candidate[1..end]);
            if leading_stamp(&bracketed).is_some() {
                rest = &candidate[end + 1..];
                if out.ends_with(' ') && rest.starts_with(' ') {
                    rest = &rest[1..];
                }
                continue;
            }
        }
        out.push('<');
        rest = &rest[open + 1..];
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out.trim().to_owned())
}

/// Reads one `[mm:ss.xx]` or `[hh:mm:ss.xx]` tag from the front of `text`,
/// reporting how many bytes it took and the time it names in milliseconds.
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
    let first = decimal(body[..colon].trim())?;
    let mut rest = body[colon + 1..].trim();
    let mut fraction = 0i64;
    let mut has_dot_frac = false;
    if let Some(dot) = rest.find(['.', ',']) {
        let frac_part = &rest[dot + 1..];
        rest = &rest[..dot];
        if frac_part.is_empty() || !frac_part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let prefix = &frac_part[..frac_part.len().min(3)];
        let value = decimal(prefix)?;
        fraction = match prefix.len() {
            1 => value * 100,
            2 => value * 10,
            _ => value,
        };
        has_dot_frac = true;
    }
    let (minutes, whole) = match rest.split_once(':') {
        None => (first, decimal(rest)?),
        Some((mid, tail)) if has_dot_frac => {
            let mins = decimal(mid)?;
            if mins >= 60 {
                return None;
            }
            (first.checked_mul(60)?.checked_add(mins)?, decimal(tail)?)
        }
        Some((mid, tail)) => match tail.split_once(':') {
            Some((sec, colon_frac)) => {
                let mins = decimal(mid)?;
                if mins >= 60 || !matches!(colon_frac.len(), 2 | 3) {
                    return None;
                }
                let fval = decimal(colon_frac)?;
                fraction = if colon_frac.len() == 2 { fval * 10 } else { fval };
                (first.checked_mul(60)?.checked_add(mins)?, decimal(sec)?)
            }
            None => {
                if !matches!(tail.len(), 2 | 3) {
                    return None;
                }
                let fval = decimal(tail)?;
                fraction = if tail.len() == 2 { fval * 10 } else { fval };
                (first, decimal(mid)?)
            }
        },
    };
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
    let mut current_group_ts: Option<i64> = None;
    let mut next_distinct_ts: Option<i64> = None;
    for line in lines.iter_mut().rev() {
        match current_group_ts {
            None => current_group_ts = Some(line.timestamp),
            Some(group_ts) if line.timestamp < group_ts => {
                next_distinct_ts = Some(group_ts);
                current_group_ts = Some(line.timestamp);
            }
            Some(_) => {}
        }
        if let Some(next_ts) = next_distinct_ts {
            line.duration = Some((next_ts - line.timestamp).max(0));
        }
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

    #[test]
    fn utf8_bom_and_bilingual_equal_timestamps_share_next_distinct_duration() {
        let lines = parse("\u{feff}[00:01.00]jp\n[00:01.00]en\n[00:04.00]next");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line, "jp");
        assert_eq!(lines[0].duration, Some(3_000));
        assert_eq!(lines[1].line, "en");
        assert_eq!(lines[1].duration, Some(3_000));
        assert_eq!(lines[2].line, "next");
        assert_eq!(lines[2].duration, None);
    }
}
