//! Recording matching for metadata-only catalogues.

use rustlink_protocol::Track;

const VERSIONS: [&str; 12] = [
    "cover",
    "karaoke",
    "remix",
    "live",
    "nightcore",
    "slowed",
    "sped",
    "instrumental",
    "acoustic",
    "preview",
    "snippet",
    "teaser",
];

fn split_words(lower: &str) -> impl Iterator<Item = &str> {
    lower.split(|character: char| !character.is_alphanumeric()).filter(|word| !word.is_empty())
}

fn words(lower: &str) -> Vec<&str> {
    let mut out: Vec<&str> = split_words(lower).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn tokens_from_words<'a>(all: &[&'a str]) -> Vec<&'a str> {
    // Articles and initials add noise to long titles, but a recording or
    // artist can legitimately be named "X", "愛", or "A B".
    if all.iter().any(|word| word.chars().nth(1).is_some()) {
        all.iter().copied().filter(|word| word.chars().nth(1).is_some()).collect()
    } else {
        all.to_vec()
    }
}

fn tokens(lower: &str) -> Vec<&str> {
    let mut all = words(lower);
    if all.iter().any(|word| word.chars().nth(1).is_some()) {
        all.retain(|word| word.chars().nth(1).is_some());
    }
    all
}

#[inline]
fn has_word(sorted: &[&str], word: &str) -> bool {
    sorted.binary_search(&word).is_ok()
}

fn overlap(want: &[&str], got: &[&str]) -> f64 {
    if want.is_empty() {
        return 0.0;
    }
    want.iter().filter(|&&word| has_word(got, word)).count() as f64 / want.len() as f64
}

pub fn is_unknown_artist(author: &str) -> bool {
    let trimmed = author.trim();
    trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown artist")
}

/// Require title, artist and duration agreement before accepting a recording.
/// Desired metadata is normalized once per search, and candidate tokens borrow
/// from one lowercased string per field without per-word heap allocations.
pub fn match_track<'a>(want: &Track, candidates: &'a [Track]) -> Option<&'a Track> {
    let want_title_lower = want.metadata.title.to_lowercase();
    let want_title = tokens(&want_title_lower);
    if want_title.is_empty() {
        return None;
    }
    let has_artist = !is_unknown_artist(&want.metadata.author);
    let want_author_lower =
        if has_artist { want.metadata.author.to_lowercase() } else { String::new() };
    let artist_name = if has_artist { words(&want_author_lower) } else { Vec::new() };
    let want_artist = if has_artist { tokens_from_words(&artist_name) } else { Vec::new() };
    let short_artist = has_artist && want_artist.iter().all(|word| word.chars().nth(1).is_none());
    let mut best = None;
    let mut best_score = f64::NEG_INFINITY;
    for candidate in candidates {
        if candidate.metadata.is_stream {
            continue;
        }
        let delta = candidate.metadata.length.abs_diff(want.metadata.length) as f64;
        let max_delta = 15_000f64.max(want.metadata.length as f64 * 0.12);
        if want.metadata.length > 0 && (candidate.metadata.length <= 0 || delta > max_delta) {
            continue;
        }
        let title_lower = candidate.metadata.title.to_lowercase();
        let candidate_tokens = words(&title_lower);
        // A named artist prefix is metadata, not a version label: the band
        // Live must not be mistaken for a live performance of every song.
        let named_title = [" - ", " – ", " — ", " | ", ": "].iter().find_map(|separator| {
            let (artist, title) = title_lower.split_once(*separator)?;
            (!artist_name.is_empty() && words(artist) == artist_name).then_some(title)
        });
        let unrequested_version = match named_title {
            Some(rest) => {
                split_words(rest).any(|w| VERSIONS.contains(&w) && !has_word(&want_title, w))
            }
            None => VERSIONS
                .iter()
                .any(|&v| has_word(&candidate_tokens, v) && !has_word(&want_title, v)),
        };
        if unrequested_version {
            continue;
        }
        let title = overlap(&want_title, &candidate_tokens);
        if title < if has_artist { 0.6 } else { 0.8 } {
            continue;
        }
        let artist = if !has_artist {
            0.0
        } else {
            let author_lower = candidate.metadata.author.to_lowercase();
            let author_tokens = words(&author_lower);
            if short_artist && named_title.is_none() {
                // A short artist such as "A" cannot be proven by an article in
                // "A Song". Require the author field or an explicit artist prefix.
                overlap(&want_artist, &author_tokens)
            } else if want_artist.is_empty() {
                0.0
            } else {
                want_artist
                    .iter()
                    .filter(|&&w| has_word(&candidate_tokens, w) || has_word(&author_tokens, w))
                    .count() as f64
                    / want_artist.len() as f64
            }
        };
        if has_artist && artist < 0.4 {
            continue;
        }
        let mut score = title + artist;
        if want.metadata.length > 0 {
            score += 1.0 - delta / max_delta;
        }
        if score > best_score {
            best_score = score;
            best = Some(candidate);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::TrackInfo;

    fn track(title: &str, author: &str, length: i64) -> Track {
        Track::new(
            String::new(),
            TrackInfo { title: title.into(), author: author.into(), length, ..Default::default() },
        )
    }

    #[test]
    fn title_artist_and_duration_must_all_agree() {
        let want = track("A Song", "Someone", 200_000);
        let candidates = [track("A Song (Official Audio)", "Someone", 201_000)];
        assert!(match_track(&want, &candidates).is_some());
        assert!(match_track(&want, &[track("A Song", "Someone", 400_000)]).is_none());
        assert!(match_track(&want, &[track("A Song", "A Cover Band", 200_000)]).is_none());
        assert!(match_track(&want, &[track("Another Tune", "Someone", 200_000)]).is_none());
    }

    #[test]
    fn artist_can_appear_in_title_and_unicode_normalization_is_preserved() {
        let want = track("夜空之歌", "歌手名稱", 200_000);
        let candidates = [track("歌手名稱 夜空之歌", "Official channel", 200_001)];
        assert!(match_track(&want, &candidates).is_some());
        assert_eq!(
            overlap(&tokens(&"A Song".to_lowercase()), &tokens(&"a SONG".to_lowercase())),
            1.0
        );
        assert_eq!(overlap(&tokens(&"A Song".to_lowercase()), &tokens("nothing alike")), 0.0);
    }

    #[test]
    fn live_streams_and_extreme_duration_metadata_are_rejected_without_overflow() {
        let want = track("A Song", "Someone", i64::MAX);
        assert!(match_track(&want, &[track("A Song", "Someone", i64::MIN)]).is_none());
        let mut live = track("A Song", "Someone", 200_000);
        live.metadata.is_stream = true;
        assert!(match_track(&track("A Song", "Someone", 200_000), &[live]).is_none());
    }

    #[test]
    fn closest_recording_wins() {
        let want = track("A Song", "Someone", 200_000);
        let candidates = [track("A Song", "Someone", 210_000), track("A Song", "Someone", 200_000)];
        assert!(std::ptr::eq(match_track(&want, &candidates).unwrap(), &candidates[1]));
    }

    #[test]
    fn single_character_titles_and_artist_names_remain_matchable() {
        for (title, artist) in [("X", "A"), ("愛", "林"), ("A B", "C D")] {
            let want = track(title, artist, 200_000);
            let candidates = [track(
                &format!("{title} (Official Audio)"),
                &format!("{artist} - Topic"),
                200_001,
            )];
            assert!(match_track(&want, &candidates).is_some(), "{title} by {artist}");
        }
    }

    #[test]
    fn an_unrequested_version_or_preview_cannot_beat_the_full_recording() {
        let want = track("A Song", "Someone", 200_000);
        for version in [
            "Cover",
            "Karaoke",
            "Remix",
            "Live",
            "Nightcore",
            "Slowed",
            "Sped Up",
            "Instrumental",
            "Acoustic",
            "Preview",
            "Snippet",
            "Teaser",
        ] {
            let title = format!("A Song ({version})");
            let candidates = [
                track(&title, "Someone", 200_000),
                track("A Song (Official Audio)", "Someone - Topic", 201_000),
            ];
            assert!(
                std::ptr::eq(match_track(&want, &candidates).unwrap(), &candidates[1]),
                "{version}"
            );
            assert!(match_track(&track(&title, "Someone", 200_000), &candidates).is_some());
        }
        assert!(match_track(&want, &[track("A Song", "Someone", 30_000)]).is_none());
    }

    #[test]
    fn a_band_name_is_not_a_version_but_an_extra_live_label_still_is() {
        let want = track("Lightning Crashes", "Live", 200_000);
        let candidates =
            [track("Live - Lightning Crashes (Official Music Video)", "Live", 200_000)];
        assert!(match_track(&want, &candidates).is_some());
        assert!(match_track(&want, &[track("Live - Lightning Crashes (Live)", "Live", 200_000)])
            .is_none());
    }

    #[test]
    fn an_article_in_the_title_cannot_impersonate_a_short_artist() {
        let want = track("Song", "A", 200_000);
        assert!(match_track(&want, &[track("A Song", "Someone Else", 200_000)]).is_none());
        assert!(match_track(&want, &[track("A - Song", "Music channel", 200_000)]).is_some());
        assert!(match_track(&want, &[track("Song", "A - Topic", 200_000)]).is_some());
    }
}
