//! Rustlink API v1 data types: strict integers, three-state patches, and unknown-field rejection.

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
    #[inline]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for Int64 {
    #[inline]
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl From<Int64> for i64 {
    #[inline]
    fn from(value: Int64) -> Self {
        value.0
    }
}

/// Distinguishes an omitted JSON field (`Missing`) from an explicit `null` (`Null`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Patch<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> Patch<T> {
    #[inline]
    pub const fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
    #[inline]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
    #[inline]
    pub const fn is_present(&self) -> bool {
        !self.is_missing()
    }
    #[inline]
    pub const fn value(&self) -> Option<&T> {
        match self {
            Self::Value(v) => Some(v),
            Self::Missing | Self::Null => None,
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Patch<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Option::<T>::deserialize(d).map(|opt| opt.map_or(Self::Null, Self::Value))
    }
}

impl<T: Serialize> Serialize for Patch<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing | Self::Null => s.serialize_none(),
            Self::Value(v) => v.serialize(s),
        }
    }
}

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
        Self { token, metadata, extra: Object::new(), user_data: Object::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackInfo {
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub is_seekable: bool,
    #[serde(default)]
    pub author: String,
    #[serde(default, rename = "duration_ms")]
    pub length: i64,
    #[serde(default)]
    pub is_stream: bool,
    #[serde(default, rename = "position_ms")]
    pub position: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub isrc: Option<String>,
    #[serde(default, rename = "source")]
    pub source_name: String,
}

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
        Self {
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
    pub fn common(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: "common".into(),
            cause: "rustlink.source".into(),
            cause_stack_trace: String::new(),
        }
    }

    pub fn fault(message: impl Into<String>, cause: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: "fault".into(),
            cause: cause.into(),
            cause_stack_trace: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoadResult {
    Track(Box<Track>),
    Playlist(Box<Playlist>),
    Search(Vec<Track>),
    Empty,
    Error(Box<Exception>),
}

impl LoadResult {
    #[inline]
    pub const fn load_type(&self) -> &'static str {
        match self {
            Self::Track(_) => "track",
            Self::Playlist(_) => "playlist",
            Self::Search(_) => "search",
            Self::Empty => "empty",
            Self::Error(_) => "error",
        }
    }

    pub fn from_error(err: impl fmt::Display) -> Self {
        Self::Error(Box::new(Exception::common(err.to_string())))
    }

    #[inline]
    pub fn track(self) -> Option<Track> {
        match self {
            Self::Track(t) => Some(*t),
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
            Self::Track(t) => m.serialize_entry("result", t)?,
            Self::Playlist(p) => m.serialize_entry("result", p)?,
            Self::Search(ts) => m.serialize_entry("result", ts)?,
            Self::Empty => m.serialize_entry("result", &Value::Null)?,
            Self::Error(e) => m.serialize_entry("result", e)?,
        }
        m.end()
    }
}

fn parse_stream<'de, A: de::MapAccess<'de>>(
    kind: &str,
    map: &mut A,
) -> Result<LoadResult, A::Error> {
    Ok(match kind {
        "track" => LoadResult::Track(Box::new(map.next_value()?)),
        "playlist" => LoadResult::Playlist(Box::new(map.next_value()?)),
        "search" => LoadResult::Search(map.next_value()?),
        "empty" => {
            let _: Option<de::IgnoredAny> = map.next_value()?;
            LoadResult::Empty
        }
        "error" => LoadResult::Error(Box::new(map.next_value()?)),
        other => return Err(de::Error::custom(format_args!("unknown load type {other}"))),
    })
}

fn parse_buffered<E: de::Error>(kind: &str, buffered: Option<Value>) -> Result<LoadResult, E> {
    if kind == "empty" {
        return Ok(LoadResult::Empty);
    }
    if !matches!(kind, "track" | "playlist" | "search" | "error") {
        return Err(E::custom(format_args!("unknown load type {kind}")));
    }
    let data = buffered.ok_or_else(|| E::missing_field("result"))?;
    Ok(match kind {
        "track" => LoadResult::Track(Box::new(serde_json::from_value(data).map_err(E::custom)?)),
        "playlist" => {
            LoadResult::Playlist(Box::new(serde_json::from_value(data).map_err(E::custom)?))
        }
        "search" => LoadResult::Search(serde_json::from_value(data).map_err(E::custom)?),
        _ => LoadResult::Error(Box::new(serde_json::from_value(data).map_err(E::custom)?)),
    })
}

impl<'de> Deserialize<'de> for LoadResult {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Field {
            Kind,
            Result,
        }

        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = LoadResult;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a LoadResult object with kind and result fields")
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let (mut kind, mut parsed, mut buffered) = (None::<String>, None, None);
                while let Some(field) = map.next_key::<Field>()? {
                    match field {
                        Field::Kind if kind.is_some() => {
                            return Err(de::Error::duplicate_field("kind"));
                        }
                        Field::Kind => kind = Some(map.next_value()?),
                        Field::Result if parsed.is_some() || buffered.is_some() => {
                            return Err(de::Error::duplicate_field("result"));
                        }
                        Field::Result => match kind.as_deref() {
                            Some(k) => parsed = Some(parse_stream(k, &mut map)?),
                            None => buffered = Some(map.next_value()?),
                        },
                    }
                }
                let kind = kind.ok_or_else(|| de::Error::missing_field("kind"))?;
                match parsed {
                    Some(done) => Ok(done),
                    None => parse_buffered(&kind, buffered),
                }
            }
        }

        d.deserialize_struct("LoadResult", &["kind", "result"], Visitor)
    }
}

fn wipe_string(secret: &mut String) {
    // SAFETY: Overwriting UTF-8 bytes with ASCII NUL (0) preserves UTF-8 validity before `clear()`.
    unsafe {
        for byte in secret.as_bytes_mut() {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    secret.clear();
}

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

impl Drop for VoiceState {
    fn drop(&mut self) {
        wipe_string(&mut self.token);
        wipe_string(&mut self.session_id);
    }
}

impl fmt::Debug for VoiceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceState")
            .field("token", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .field("session_id", &self.session_id)
            .field("channel_id", &self.channel_id)
            .finish()
    }
}

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
        Self { time: 0, position: 0, connected: false, ping: -1 }
    }
}

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
        Self {
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

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackUpdate {
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    pub token: Patch<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_data: Option<Object>,
}

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
        assert!(u.end_time.is_missing() && u.track_update().token.is_missing());
        let u: PlayerUpdate =
            serde_json::from_value(json!({"end_ms": null, "track": null})).unwrap();
        assert!(u.end_time.is_null() && u.track_update().token.is_null());
        let u: PlayerUpdate =
            serde_json::from_value(json!({"end_ms": 5000, "track": {"query": "ytsearch:music"}}))
                .unwrap();
        assert_eq!(u.end_time.value().unwrap().0, 5000);
        assert_eq!(u.track_update().query.as_deref(), Some("ytsearch:music"));

        let empty_json = serde_json::to_value(PlayerUpdate::default()).unwrap();
        assert_eq!(empty_json, json!({}));
        let roundtrip: PlayerUpdate = serde_json::from_value(empty_json).unwrap();
        assert!(roundtrip.track.is_missing() && roundtrip.end_time.is_missing());

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
        assert!(rendered.contains("<redacted>") && !rendered.contains("secret-voice-token"));
    }

    #[test]
    fn responses_use_native_shapes() {
        assert_eq!(
            serde_json::to_value(LoadResult::Empty).unwrap(),
            json!({"kind":"empty", "result":null})
        );
        let value =
            serde_json::to_value(Track::new("rlt1.test".into(), TrackInfo::default())).unwrap();
        assert_eq!(value["token"], "rlt1.test");
        assert_eq!(value["metadata"]["duration_ms"], 0);
        assert!(value.get("encoded").is_none());
    }

    #[test]
    fn load_result_requires_result_field_unless_empty() {
        for body in [json!({"kind": "empty"}), json!({"result": null, "kind": "empty"})] {
            assert_eq!(serde_json::from_value::<LoadResult>(body).unwrap(), LoadResult::Empty);
        }
        for kind in ["track", "playlist", "search", "error"] {
            let err = serde_json::from_value::<LoadResult>(json!({"kind": kind}))
                .unwrap_err()
                .to_string();
            assert!(err.contains("missing field `result`"), "{kind}: {err}");
        }
        assert_eq!(
            serde_json::from_value::<LoadResult>(json!({"result": [], "kind": "search"})).unwrap(),
            LoadResult::Search(Vec::new())
        );
    }

    #[test]
    fn wipe_string_clears_secret_buffer() {
        let mut token = String::from("discord-voice-token");
        wipe_string(&mut token);
        assert!(token.is_empty());
        drop(VoiceState {
            token: "secret".into(),
            endpoint: "voice.discord.media:443".into(),
            session_id: "sess".into(),
            channel_id: Some("1".into()),
        });
    }
}
