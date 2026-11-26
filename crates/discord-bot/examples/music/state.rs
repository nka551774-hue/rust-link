//! Per-guild playback queue and pure formatting/parsing helpers.

use std::collections::VecDeque;

use rustlink_client::Track;

struct QueuedItem {
    track: Track,
    start_ms: Option<i64>,
}

/// Active track and waiting queue for one guild.
pub struct Guild {
    queue: VecDeque<QueuedItem>,
    current: Option<Track>,
    current_play_id: Option<u64>,
    next_play_id: u64,
    failures: u32,
    pub volume: i64,
}

/// Consecutive load failures tolerated before clearing the queue.
pub const MAX_FAILURES_IN_A_ROW: u32 = 3;

impl Guild {
    pub fn new(volume: i64) -> Guild {
        Guild {
            queue: VecDeque::new(),
            current: None,
            current_play_id: None,
            next_play_id: 0,
            failures: 0,
            volume,
        }
    }

    #[allow(dead_code)]
    pub fn enqueue(&mut self, tracks: Vec<Track>, max: usize) -> usize {
        self.enqueue_with_start(tracks, None, max)
    }

    pub fn enqueue_with_start(
        &mut self,
        tracks: Vec<Track>,
        first_start_ms: Option<i64>,
        max: usize,
    ) -> usize {
        let accepted = tracks.len().min(max.saturating_sub(self.queue.len()));
        for (idx, track) in tracks.into_iter().take(accepted).enumerate() {
            self.queue.push_back(QueuedItem {
                track,
                start_ms: (idx == 0).then_some(first_start_ms).flatten(),
            });
        }
        accepted
    }

    /// Puts a replacement for the track that just failed at the head of the queue.
    pub fn retry_with(&mut self, track: Track) {
        self.queue.push_front(QueuedItem { track, start_ms: None });
    }

    #[allow(dead_code)]
    pub fn advance(&mut self) -> Option<Track> {
        self.advance_with_start().map(|(track, _)| track)
    }

    pub fn advance_with_start(&mut self) -> Option<(Track, Option<i64>)> {
        let Some(item) = self.queue.pop_front() else {
            self.finish();
            return None;
        };
        self.next_play_id = self.next_play_id.wrapping_add(1).max(1);
        self.current_play_id = Some(self.next_play_id);
        let cloned = item.track.clone();
        self.current = Some(item.track);
        Some((cloned, item.start_ms))
    }

    pub fn current_play_id(&self) -> Option<u64> {
        self.current_play_id
    }

    pub fn finish(&mut self) {
        self.current = None;
        self.current_play_id = None;
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.finish();
    }

    pub fn record(&mut self, loaded: bool) -> bool {
        self.failures = if loaded { 0 } else { self.failures + 1 };
        if self.failures < MAX_FAILURES_IN_A_ROW {
            return false;
        }
        self.failures = 0;
        self.clear();
        true
    }

    pub fn reset_failures(&mut self) {
        self.failures = 0;
    }

    pub fn playing(&self) -> bool {
        self.current.is_some()
    }

    pub fn busy(&self) -> bool {
        self.current.is_some() || !self.queue.is_empty()
    }

    pub fn current(&self) -> Option<&Track> {
        self.current.as_ref()
    }

    pub fn waiting(&self) -> usize {
        self.queue.len()
    }

    pub fn upcoming(&self, limit: usize) -> impl Iterator<Item = &Track> {
        self.queue.iter().take(limit).map(|item| &item.track)
    }
}

pub fn normalize_query(text: &str) -> std::borrow::Cow<'_, str> {
    rustlink_discord_bot::normalize_query(text)
}

pub fn format_length(milliseconds: i64, stream: bool) -> String {
    if stream {
        return "直播".into();
    }
    if milliseconds <= 0 {
        return "未知長度".into();
    }
    let s = milliseconds / 1000;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn parse_volume(text: &str) -> Option<i64> {
    text.trim().trim_end_matches('%').trim().parse().ok().filter(|v| (0..=200).contains(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlink_protocol::TrackInfo;

    fn track(name: &str) -> Track {
        Track::new(format!("rlt1.{name}"), TrackInfo { title: name.into(), ..Default::default() })
    }

    #[test]
    fn tracks_play_in_the_order_they_were_added() {
        let mut guild = Guild::new(100);
        assert!(!guild.busy());
        assert_eq!(guild.enqueue(vec![track("a"), track("b"), track("c")], 10), 3);
        assert!(guild.busy() && !guild.playing());
        assert_eq!(guild.advance().unwrap().metadata.title, "a");
        assert!(guild.playing() && guild.waiting() == 2);
        assert_eq!(guild.upcoming(1).next().unwrap().metadata.title, "b");
        guild.finish();
        assert_eq!(guild.advance().unwrap().metadata.title, "b");
        assert_eq!(guild.advance().unwrap().metadata.title, "c");
        assert!(guild.advance().is_none() && !guild.busy());
    }

    #[test]
    fn a_run_of_failed_tracks_gives_up_on_the_queue() {
        let mut guild = Guild::new(100);
        guild.enqueue((0..10).map(|n| track(&n.to_string())).collect(), 20);
        guild.advance();
        assert!(!guild.record(false) && !guild.record(false));
        assert!(!guild.record(true));
        assert!(!guild.record(false) && !guild.record(false));
        assert!(guild.record(false) && !guild.busy());
    }

    #[test]
    fn the_queue_never_grows_past_its_limit() {
        let mut guild = Guild::new(100);
        assert_eq!(guild.enqueue(vec![track("a"), track("b"), track("c")], 2), 2);
        assert_eq!(guild.enqueue(vec![track("d")], 2), 0);
        guild.advance();
        assert_eq!(guild.enqueue(vec![track("e"), track("f")], 2), 1);
        guild.clear();
        assert!(!guild.busy() && guild.waiting() == 0);
    }

    #[test]
    fn plain_words_become_a_search_and_everything_else_is_left_alone() {
        assert_eq!(normalize_query("  lofi hip hop "), "ytsearch:lofi hip hop");
        for same in [
            "https://youtu.be/dQw4w9WgXcQ",
            "<https://www.youtube.com/watch?v=dQw4w9WgXcQ>",
            "dQw4w9WgXcQ",
            "ytsearch:already",
            "scsearch:cloud",
            "spotify:track:4cOdK2wGLETKBW3PvgPWqT",
            "youtube:playlist:PL123",
            "hello-world",
            "",
        ] {
            assert_eq!(normalize_query(same), same);
        }
        assert_eq!(normalize_query("hello-worl"), "ytsearch:hello-worl");
    }

    #[test]
    fn lengths_and_volumes_read_naturally() {
        assert_eq!(format_length(225_000, false), "3:45");
        assert_eq!(format_length(3_723_000, false), "1:02:03");
        assert_eq!(format_length(0, false), "未知長度");
        assert_eq!(format_length(5, true), "直播");
        assert_eq!(parse_volume(" 80% "), Some(80));
        assert_eq!(parse_volume("0"), Some(0));
        assert_eq!(parse_volume("201"), None);
        assert_eq!(parse_volume("-1"), None);
        assert_eq!(parse_volume("loud"), None);
    }
}
