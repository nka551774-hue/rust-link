//! Rustlink API v1 data types. JSON numbers are strict integers; patches
//! distinguish omitted fields from null. Unknown update fields are rejected.

use std::fmt;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Application metadata carried through unchanged.
pub type Object = Map<String, Value>;

/// A strict JSON integer, with no string or floating-point coercion.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Int64(pub i64);

impl Int64 {
    pub fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for Int64 {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

/// A field that distinguishes an omitted value from an explicit `null`.
///
/// The protocol leans on that difference: an absent `end_ms` leaves the current
/// limit alone while `"end_ms": null` clears it, and the same holds for the
/// track token in a player update.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Patch<T> {
    /// The client did not mention the field.
    #[default]
    Missing,
    /// The client sent an explicit `null`.
    Null,
    /// The client sent a value.
    Value(T),
}

impl<T> Patch<T> {
    pub fn is_missing(&self) -> bool {
        matches!(self, Patch::Missing)
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Patch::Null)
    }
    pub fn is_present(&self) -> bool {
        !self.is_missing()
    }
    pub fn value(&self) -> Option<&T> {
        match self {
            Patch::Value(v) => Some(v),
            _ => None,
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Patch<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Option::<Value>::deserialize(d)?;
        match v {
            None => Ok(Patch::Null),
            Some(Value::Null) => Ok(Patch::Null),
            Some(other) => T::deserialize(other).map(Patch::Value).map_err(de::Error::custom),
        }
    }
}

impl<T: Serialize> Serialize for Patch<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Patch::Missing | Patch::Null => s.serialize_none(),
            Patch::Value(v) => v.serialize(s),
        }
    }
}

/// One playable track: the encoded form, the metadata, and the two bags a client
/// may put its own values in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub token: String,
    pub metadata: TrackInfo,
    #[serde(default)]
    pub extra: Object,
    #[serde(default)]
    pub user_data: Object,
}

impl Track {
    pub fn new(token: String, metadata: TrackInfo) -> Self {
        Track { token, metadata, extra: Object::new(), user_data: Object::new() }
    }
}

/// The metadata a client shows and uses to decide what to do next.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackInfo {
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub is_seekable: bool,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    #[serde(rename = "duration_ms")]
    pub length: i64,
    #[serde(default)]
    pub is_stream: bool,
    #[serde(default)]
    #[serde(rename = "position_ms")]
    pub position: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub isrc: Option<String>,
    #[serde(default)]
    #[serde(rename = "source")]
    pub source_name: String,
}

/// A playlist as `tracks/resolve` returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    pub metadata: PlaylistInfo,
    #[serde(default)]
    pub extra: Object,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

impl Playlist {
    pub fn new(name: impl Into<String>, selected_track: i64, tracks: Vec<Track>) -> Self {
        Playlist {
            metadata: PlaylistInfo { name: name.into(), selected_track },
            extra: Object::new(),
            tracks,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub selected_track: i64,
}

/// A failure a source reports, which travels back inside a load result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exception {
    pub message: String,
    pub severity: String,
    pub cause: String,
    #[serde(default)]
    pub cause_stack_trace: String,
}

impl Exception {
    /// The severity a source failure carries: the node is fine, the lookup was not.
    pub fn common(message: impl Into<String>) -> Self {
        Exception {
            message: message.into(),
            severity: "common".into(),
            cause: "rustlink.source".into(),
            cause_stack_trace: String::new(),
        }
    }

    /// The severity a playback failure carries.
    pub fn fault(message: impl Into<String>, cause: impl Into<String>) -> Self {
        Exception {
            message: message.into(),
            severity: "fault".into(),
            cause: cause.into(),
            cause_stack_trace: String::new(),
        }
    }
}

/// The four successful answers plus the failure, which is what `tracks/resolve`
/// replies with.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadResult {
    Track(Box<Track>),
    Playlist(Box<Playlist>),
    Search(Vec<Track>),
    Empty,
    Error(Box<Exception>),
}

impl LoadResult {
    pub fn load_type(&self) -> &'static str {
        match self {
            LoadResult::Track(_) => "track",
            LoadResult::Playlist(_) => "playlist",
            LoadResult::Search(_) => "search",
            LoadResult::Empty => "empty",
            LoadResult::Error(_) => "error",
        }
    }

    /// Wraps a source failure in the shape a client expects from `tracks/resolve`.
    pub fn from_error(err: impl std::fmt::Display) -> Self {
        LoadResult::Error(Box::new(Exception::common(err.to_string())))
    }

    pub fn track(self) -> Option<Track> {
        match self {
            LoadResult::Track(t) => Some(*t),
            _ => None,
        }
    }
}

impl Serialize for LoadResult {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(2))?;
        m.serialize_entry("kind", self.load_type())?;
        match self {
            LoadResult::Track(t) => m.serialize_entry("result", t)?,
            LoadResult::Playlist(p) => m.serialize_entry("result", p)?,
            LoadResult::Search(ts) => m.serialize_entry("result", ts)?,
            LoadResult::Empty => m.serialize_entry("result", &Value::Null)?,
            LoadResult::Error(e) => m.serialize_entry("result", e)?,
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for LoadResult {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawLoadResult::deserialize(d)?;
        let data = raw.data.unwrap_or(Value::Null);
        let convert = |e: serde_json::Error| de::Error::custom(e.to_string());
        Ok(match raw.load_type.as_str() {
            "track" => LoadResult::Track(Box::new(serde_json::from_value(data).map_err(convert)?)),
            "playlist" => {
                LoadResult::Playlist(Box::new(serde_json::from_value(data).map_err(convert)?))
            }
            "search" => LoadResult::Search(serde_json::from_value(data).map_err(convert)?),
            "empty" => LoadResult::Empty,
            "error" => LoadResult::Error(Box::new(serde_json::from_value(data).map_err(convert)?)),
            other => return Err(de::Error::custom(format!("unknown load type {other}"))),
        })
    }
}

#[derive(Deserialize)]
struct RawLoadResult {
    #[serde(rename = "kind")]
    load_type: String,
    #[serde(default)]
    #[serde(rename = "result")]
    data: Option<Value>,
}

/// What a bot reports about its voice connection, which the node needs before it
/// can open one of its own.
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceState {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub channel_id: Option<String>,
}

impl fmt::Debug for VoiceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VoiceState")
            .field("token", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .field("session_id", &self.session_id)
            .field("channel_id", &self.channel_id)
            .finish()
    }
}

/// The playback position and link quality a client sees in `player.updated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerState {
    #[serde(rename = "timestamp_ms")]
    pub time: i64,
    #[serde(rename = "position_ms")]
    pub position: i64,
    pub connected: bool,
    #[serde(rename = "ping_ms")]
    pub ping: i64,
}

impl Default for PlayerState {
    fn default() -> Self {
        PlayerState { time: 0, position: 0, connected: false, ping: -1 }
    }
}

/// The player body, which is both what `GET` returns and what `PATCH` answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Player {
    pub guild_id: String,
    pub track: Option<Track>,
    pub volume: i64,
    pub paused: bool,
    pub state: PlayerState,
    pub voice: VoiceState,
    pub filters: Object,
}

impl Player {
    pub fn new(guild_id: impl Into<String>) -> Self {
        Player {
            guild_id: guild_id.into(),
            track: None,
            volume: 100,
            paused: false,
            state: PlayerState::default(),
            voice: VoiceState::default(),
            filters: Object::new(),
        }
    }
}

/// The `track` object of a player update.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackUpdate {
    #[serde(default)]
    #[serde(skip_serializing_if = "Patch::is_missing")]
    pub token: Patch<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_data: Option<Object>,
}

/// Partial player state. `track: null` stops playback; omission preserves it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerUpdate {
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    pub track: Patch<TrackUpdate>,
    #[serde(default, rename = "position_ms", skip_serializing_if = "Option::is_none")]
    pub position: Option<Int64>,
    #[serde(default, rename = "end_ms", skip_serializing_if = "Patch::is_missing")]
    pub end_time: Patch<Int64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<Int64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<VoiceState>,
}

impl PlayerUpdate {
    pub fn track_update(&self) -> TrackUpdate {
        match &self.track {
            Patch::Value(track) => track.clone(),
            Patch::Null => TrackUpdate { token: Patch::Null, ..Default::default() },
            Patch::Missing => TrackUpdate::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_are_strict_and_bounded() {
        assert_eq!(serde_json::from_str::<Int64>("9223372036854775807").unwrap().0, i64::MAX);
        for raw in ["\"12\"", "12.0", "1.5", "9223372036854775808", "null"] {
            assert!(serde_json::from_str::<Int64>(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn patch_preserves_missing_null_and_value() {
        let u: PlayerUpdate = serde_json::from_value(json!({})).unwrap();
        assert!(u.end_time.is_missing());
        assert!(u.track_update().token.is_missing());
        let u: PlayerUpdate =
            serde_json::from_value(json!({"end_ms": null, "track": null})).unwrap();
        assert!(u.end_time.is_null());
        assert!(u.track_update().token.is_null());
        let u: PlayerUpdate =
            serde_json::from_value(json!({"end_ms": 5000, "track": {"query": "ytsearch:music"}}))
                .unwrap();
        assert_eq!(u.end_time.value().unwrap().0, 5000);
        assert_eq!(u.track_update().query.as_deref(), Some("ytsearch:music"));
        let roundtrip: PlayerUpdate =
            serde_json::from_value(serde_json::to_value(PlayerUpdate::default()).unwrap()).unwrap();
        assert!(roundtrip.track.is_missing());
        assert!(roundtrip.end_time.is_missing());
        assert_eq!(serde_json::to_value(PlayerUpdate::default()).unwrap(), json!({}));
        let update = PlayerUpdate {
            track: Patch::Value(TrackUpdate {
                token: Patch::Value("rlt1.example".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(update).unwrap(),
            json!({"track": {"token": "rlt1.example"}})
        );
    }

    #[test]
    fn unknown_fields_and_typos_are_rejected() {
        for body in [
            json!({"unknown_track_field":"abc"}),
            json!({"unknown_flag":true}),
            json!({"track":{"unknown_token":"abc"}}),
            json!({"postion_ms":0}),
        ] {
            assert!(serde_json::from_value::<PlayerUpdate>(body).is_err());
        }
    }

    #[test]
    fn voice_state_debug_redacts_the_server_token() {
        let voice = VoiceState {
            token: "secret-voice-token".into(),
            endpoint: "voice.discord.media:443".into(),
            session_id: "session".into(),
            channel_id: Some("123".into()),
        };
        let rendered = format!("{voice:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("secret-voice-token"));
    }

    #[test]
    fn responses_use_native_shapes() {
        assert_eq!(
            serde_json::to_value(LoadResult::Empty).unwrap(),
            json!({"kind":"empty", "result":null})
        );
        let track = Track::new("rlt1.test".into(), TrackInfo::default());
        let value = serde_json::to_value(track).unwrap();
        assert_eq!(value["token"], "rlt1.test");
        assert_eq!(value["metadata"]["duration_ms"], 0);
        assert!(value.get("encoded").is_none());
    }
}
