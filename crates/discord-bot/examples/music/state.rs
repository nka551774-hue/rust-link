//! The per-guild queue and the small pure helpers the commands rely on.

use std::collections::VecDeque;

use rustlink_client::Track;

struct QueuedItem {
    track: Track,
    start_ms: Option<i64>,
}

/// What one guild is playing and what is waiting.
pub struct Guild {
    queue: VecDeque<QueuedItem>,
    current: Option<Track>,
    current_play_id: Option<u64>,
    next_play_id: u64,
    /// Tracks in a row that could not be loaded; a working one resets it.
    failures: u32,
    pub volume: i64,
}

/// How many tracks in a row may fail before the queue is given up on. When
/// every track fails (YouTube refusing the host, say), playing on through a
/// playlist only fills the log with the same error.
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

    /// Adds tracks up to `max` waiting, and returns how many were accepted.
    #[allow(dead_code)]
    pub fn enqueue(&mut self, tracks: Vec<Track>, max: usize) -> usize {
        self.enqueue_with_start(tracks, None, max)
    }

    /// Adds tracks up to `max` waiting, attaching `first_start_ms` to the first
    /// track when a single-video timestamp link was requested.
    pub fn enqueue_with_start(
        &mut self,
        tracks: Vec<Track>,
        first_start_ms: Option<i64>,
        max: usize,
    ) -> usize {
        let room = max.saturating_sub(self.queue.len());
        let accepted = tracks.len().min(room);
        for (idx, track) in tracks.into_iter().take(accepted).enumerate() {
            self.queue.push_back(QueuedItem {
                track,
                start_ms: (idx == 0).then_some(first_start_ms).flatten(),
            });
        }
        accepted
    }

    /// Moves the next waiting track into `current` and returns it.
    #[allow(dead_code)]
    pub fn advance(&mut self) -> Option<Track> {
        self.advance_with_start().map(|(track, _)| track)
    }

    /// Moves the next waiting track into `current` and returns `(track, start_ms)`.
    pub fn advance_with_start(&mut self) -> Option<(Track, Option<i64>)> {
        match self.queue.pop_front() {
            Some(item) => {
                self.next_play_id = self.next_play_id.wrapping_add(1).max(1);
                self.current_play_id = Some(self.next_play_id);
                let cloned = item.track.clone();
                self.current = Some(item.track);
                Some((cloned, item.start_ms))
            }
            None => {
                self.current = None;
                self.current_play_id = None;
                None
            }
        }
    }

    pub fn current_play_id(&self) -> Option<u64> {
        self.current_play_id
    }

    /// The current track is over (finished, failed or skipped).
    pub fn finish(&mut self) {
        self.current = None;
        self.current_play_id = None;
    }

    /// Forgets everything, as `/stop` and being removed from the channel do.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.current = None;
        self.current_play_id = None;
    }

    /// Records how the current track ended. Returns true once
    /// [`MAX_FAILURES_IN_A_ROW`] have failed in a row, when the queue is
    /// cleared rather than tried further.
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

    /// Anything playing or waiting, which is what keeps the bot in the channel.
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

/// Turns what a person typed into something Rustlink can resolve. A link, a
/// video ID or a source prefix is passed through; plain words become a YouTube
/// search, because Rustlink answers `empty` to text it cannot route.
pub fn normalize_query(text: &str) -> std::borrow::Cow<'_, str> {
    rustlink_discord_bot::normalize_query(text)
}

/// `3:45` or `1:02:03`; a stream and an unknown length are named instead.
pub fn format_length(milliseconds: i64, stream: bool) -> String {
    if stream {
        return "直播".into();
    }
    if milliseconds <= 0 {
        return "未知長度".into();
    }
    let seconds = milliseconds / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Reads the `/volume` argument, which Discord delivers as text.
pub fn parse_volume(text: &str) -> Option<i64> {
    text.trim()
        .trim_end_matches('%')
        .trim()
        .parse()
        .ok()
        .filter(|volume| (0..=200).contains(volume))
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
        assert!(guild.advance().is_none() && !guild.busy(), "an empty queue ends the playback");
    }

    #[test]
    fn a_run_of_failed_tracks_gives_up_on_the_queue() {
        let mut guild = Guild::new(100);
        guild.enqueue((0..10).map(|n| track(&n.to_string())).collect(), 20);
        guild.advance();
        assert!(!guild.record(false) && !guild.record(false), "two failures are tolerated");
        assert!(!guild.record(true), "a track that plays resets the count");
        assert!(!guild.record(false) && !guild.record(false));
        assert!(guild.record(false), "the third in a row stops the queue");
        assert!(!guild.busy(), "nothing is left to fail");
    }

    #[test]
    fn the_queue_never_grows_past_its_limit() {
        let mut guild = Guild::new(100);
        assert_eq!(guild.enqueue(vec![track("a"), track("b"), track("c")], 2), 2);
        assert_eq!(guild.enqueue(vec![track("d")], 2), 0, "full");
        guild.advance();
        assert_eq!(
            guild.enqueue(vec![track("e"), track("f")], 2),
            1,
            "the playing track is not waiting"
        );
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
        ] {
            assert_eq!(normalize_query(same), same);
        }
        assert_eq!(
            normalize_query("hello-worl"),
            "ytsearch:hello-worl",
            "10 characters is not a video ID"
        );
        // Rustlink itself routes any 11 characters of [A-Za-z0-9_-] to YouTube as a video ID.
        assert_eq!(normalize_query("hello-world"), "hello-world");
        assert_eq!(normalize_query(""), "");
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
