//! Recording matching for metadata-only catalogues.

use std::collections::HashSet;

use rustlink_protocol::Track;

fn words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn tokens(text: &str) -> HashSet<String> {
    let all = words(text);
    let meaningful: HashSet<String> =
        all.iter().filter(|word| word.chars().nth(1).is_some()).cloned().collect();
    // Articles and initials add noise to long titles, but a recording or
    // artist can legitimately be named "X", "愛", or "A B".
    if meaningful.is_empty() {
        all
    } else {
        meaningful
    }
}

fn overlap(want: &HashSet<String>, got: &HashSet<String>) -> f64 {
    if want.is_empty() {
        return 0.0;
    }
    want.iter().filter(|word| got.contains(*word)).count() as f64 / want.len() as f64
}

/// Require title, artist and duration agreement before accepting a recording.
/// Desired metadata is normalized once per search, and candidate title tokens
/// are reused for artist matching without formatting another temporary string.
pub fn match_track<'a>(want: &Track, candidates: &'a [Track]) -> Option<&'a Track> {
    let want_title = tokens(&want.metadata.title);
    let want_artist = tokens(&want.metadata.author);
    let artist_name = words(&want.metadata.author);
    let short_artist = want_artist.iter().all(|word| word.chars().nth(1).is_none());
    let mut best = None;
    let mut best_score = 0.0;
    for candidate in candidates {
        if candidate.metadata.is_stream {
            continue;
        }
        let delta = candidate.metadata.length.abs_diff(want.metadata.length) as f64;
        if want.metadata.length > 0
            && (candidate.metadata.length <= 0
                || delta > 15_000f64.max(want.metadata.length as f64 * 0.12))
        {
            continue;
        }
        let mut candidate_tokens = words(&candidate.metadata.title);
        // A named artist prefix is metadata, not a version label: the band
        // Live must not be mistaken for a live performance of every song.
        let named_title = [" - ", " – ", " — ", " | ", ": "].iter().find_map(|separator| {
            let (artist, title) = candidate.metadata.title.split_once(*separator)?;
            (!artist_name.is_empty() && words(artist) == artist_name).then_some(title)
        });
        let version_tokens = named_title.map(words);
        let version_tokens = version_tokens.as_ref().unwrap_or(&candidate_tokens);
        if [
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
        ]
        .iter()
        .any(|version| version_tokens.contains(*version) && !want_title.contains(*version))
        {
            continue;
        }
        let title = overlap(&want_title, &candidate_tokens);
        if title < 0.6 {
            continue;
        }
        let author_tokens = words(&candidate.metadata.author);
        let artist = if short_artist && named_title.is_none() {
            // A short artist such as "A" cannot be proven by an article in
            // "A Song". Require the author field or an explicit artist prefix.
            overlap(&want_artist, &author_tokens)
        } else {
            candidate_tokens.extend(author_tokens);
            overlap(&want_artist, &candidate_tokens)
        };
        if artist < 0.4 {
            continue;
        }
        let mut score = title + artist;
        if want.metadata.length > 0 {
            score += 1.0 - delta / want.metadata.length as f64;
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
        assert_eq!(overlap(&tokens("A Song"), &tokens("a SONG")), 1.0);
        assert_eq!(overlap(&tokens("A Song"), &tokens("nothing alike")), 0.0);
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
