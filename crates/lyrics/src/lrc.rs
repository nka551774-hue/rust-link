//! Bounded LRC timed-lyrics parser.

use std::borrow::Cow;

use crate::{trim_owned, Line};
use rustlink_protocol::Object;

const MAX_LINES: usize = 10_000;
const MAX_TEXT_BYTES: usize = 8 << 20;

pub(crate) fn has_lines(text: &str) -> bool {
    let mut found = false;
    scan_lrc(text, |_, _, _| found = true) && found
}

pub fn parse(text: &str) -> Vec<Line> {
    let (mut lines, mut stamps) = (Vec::new(), Vec::new());
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
    lines.sort_by_key(|l| l.timestamp);
    fill_durations(&mut lines);
    lines
}

fn scan_lrc(text: &str, mut on_line: impl FnMut(&str, usize, usize)) -> bool {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (mut line_count, mut text_bytes) = (0usize, 0usize);
    for raw in text.split('\n') {
        let trimmed = raw.trim_end_matches('\r').trim_start_matches([' ', '\t']);
        let (mut rest, mut stamps) = (trimmed, 0usize);
        while let Some((taken, _)) = leading_stamp(rest) {
            stamps += 1;
            rest = rest[taken..].trim_start_matches([' ', '\t']);
        }
        if stamps == 0 {
            continue;
        }
        line_count = line_count.saturating_add(stamps);
        text_bytes = text_bytes.saturating_add(stamps.saturating_mul(rest.trim().len()));
        if line_count > MAX_LINES || text_bytes > MAX_TEXT_BYTES {
            return false;
        }
        on_line(trimmed, trimmed.len() - rest.len(), stamps);
    }
    true
}

fn strip_inline_stamps(words: &str) -> Cow<'_, str> {
    if !words.contains('<') {
        return Cow::Borrowed(words);
    }
    let mut out = String::with_capacity(words.len());
    let mut rest = words;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let cand = &rest[open..];
        if let Some(end) = cand.find('>').filter(|&e| parse_stamp_body(&cand[1..e]).is_some()) {
            rest = &cand[end + 1..];
            if out.ends_with(' ') && rest.starts_with(' ') {
                rest = &rest[1..];
            }
            continue;
        }
        out.push('<');
        rest = &rest[open + 1..];
    }
    out.push_str(rest);
    Cow::Owned(trim_owned(out))
}

fn leading_stamp(text: &str) -> Option<(usize, i64)> {
    let rest = text.strip_prefix('[')?;
    let end = rest.find(']')?;
    Some((end + 2, parse_stamp_body(&rest[..end])?))
}

fn decimal(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
}

fn scale_frac(digits: &str, allow_dot_len: bool) -> Option<i64> {
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (!allow_dot_len && !matches!(digits.len(), 2 | 3))
    {
        return None;
    }
    let prefix = &digits[..digits.len().min(3)];
    let v = decimal(prefix)?;
    Some(match prefix.len() {
        1 => v * 100,
        2 => v * 10,
        _ => v,
    })
}

fn parse_stamp_body(body: &str) -> Option<i64> {
    let (head, tail) = body.split_once(':')?;
    let first = decimal(head.trim())?;
    let mut rest = tail.trim();
    let (mut frac, mut has_dot) = (0i64, false);
    if let Some(dot) = rest.find(['.', ',']) {
        frac = scale_frac(&rest[dot + 1..], true)?;
        rest = &rest[..dot];
        has_dot = true;
    }
    let (minutes, whole) = match rest.split_once(':') {
        None => (first, decimal(rest)?),
        Some((mid, after)) if has_dot => {
            let mins = decimal(mid).filter(|&m| m < 60)?;
            (first.checked_mul(60)?.checked_add(mins)?, decimal(after)?)
        }
        Some((mid, after)) => match after.split_once(':') {
            Some((sec, colon_frac)) => {
                let mins = decimal(mid).filter(|&m| m < 60)?;
                frac = scale_frac(colon_frac, false)?;
                (first.checked_mul(60)?.checked_add(mins)?, decimal(sec)?)
            }
            None => {
                frac = scale_frac(after, false)?;
                (first, decimal(mid)?)
            }
        },
    };
    (whole < 60)
        .then(|| minutes.checked_mul(60_000)?.checked_add(whole * 1000)?.checked_add(frac))?
}

fn fill_durations(lines: &mut [Line]) {
    let (mut group_ts, mut next_ts) = (None, None);
    for line in lines.iter_mut().rev() {
        match group_ts {
            None => group_ts = Some(line.timestamp),
            Some(ts) if line.timestamp < ts => {
                next_ts = Some(ts);
                group_ts = Some(line.timestamp);
            }
            Some(_) => {}
        }
        if let Some(next) = next_ts {
            line.duration = Some((next - line.timestamp).max(0));
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
        assert_eq!((lines[0].timestamp, lines[0].line.as_str()), (12_340, "hello"));
    }

    #[test]
    fn a_line_with_several_tags_becomes_several_lines() {
        let lines = parse("[00:01.00][00:02.00]again");
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].timestamp, lines[1].timestamp), (1_000, 2_000));
        assert_eq!((lines[0].line.as_str(), lines[1].line.as_str()), ("again", "again"));
    }

    #[test]
    fn the_fraction_reads_as_tenths_hundredths_or_milliseconds() {
        for s in ["[00:00.5]a", "[00:00.50]a", "[00:00.500]a"] {
            assert_eq!(parse(s)[0].timestamp, 500);
        }
    }

    #[test]
    fn a_header_tag_is_not_a_line() {
        assert_eq!(parse("[ar:someone]\n[00:01.00]words").len(), 1);
    }

    #[test]
    fn a_line_out_of_order_is_sorted() {
        let lines = parse("[00:05.00]second\n[00:01.00]first");
        assert_eq!((lines[0].line.as_str(), lines[1].line.as_str()), ("first", "second"));
    }

    #[test]
    fn every_line_but_the_last_knows_how_long_it_lasts() {
        let lines = parse("[00:01.00]a\n[00:03.00]b");
        assert_eq!((lines[0].duration, lines[1].duration), (Some(2_000), None));
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
            assert_eq!((lines[0].timestamp, lines[0].line.as_str()), (1000, "valid"));
        }
        assert_eq!(parse("[153722867280912:55.807]last")[0].timestamp, i64::MAX);
    }

    #[test]
    fn repeated_timestamps_cannot_expand_an_answer_without_a_bound() {
        let large_line = format!("{}{}", "[00:01]".repeat(20), "a".repeat(512 << 10));
        assert!(large_line.len() < 1 << 20 && parse(&large_line).is_empty());
        assert!(parse(&format!("{}words", "[00:01]".repeat(MAX_LINES + 1))).is_empty());
    }

    #[test]
    fn utf8_bom_and_bilingual_equal_timestamps_share_next_distinct_duration() {
        let lines = parse("\u{feff}[00:01.00]jp\n[00:01.00]en\n[00:04.00]next");
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[0].line.as_str(), lines[0].duration), ("jp", Some(3_000)));
        assert_eq!((lines[1].line.as_str(), lines[1].duration), ("en", Some(3_000)));
        assert_eq!((lines[2].line.as_str(), lines[2].duration), ("next", None));
    }

    #[test]
    fn inline_word_timestamps_strip_cleanly_without_bracket_truncation() {
        let lines = parse("[00:01.00]<00:01.10>hello <00:01.50>world\n[00:02.00]<00:02.00]keep>");
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[0].line.as_str(), lines[1].line.as_str()),
            ("hello world", "<00:02.00]keep>")
        );
    }
}
