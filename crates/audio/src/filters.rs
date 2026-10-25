//! The ten standard filters.
//!
//! One parse serves both engines: the FFmpeg engine renders the result as a
//! filter graph, and the in-process engine applies the same settings to samples.
//! Because they read one validated request they cannot drift apart, and a
//! request one accepts is a request the other accepts.
//!
//! Nothing a client sends reaches FFmpeg as text. Every value is parsed into a
//! number and formatted again from that number, so a filter string cannot carry
//! a graph of its own.

use std::collections::BTreeMap;
use std::fmt;

use rustlink_protocol::Object;
use serde_json::Value;

/// The filter names the protocol defines, in the order the reference
/// implementation reports them.
pub const FILTER_NAMES: [&str; 10] = [
    "volume",
    "equalizer",
    "karaoke",
    "timescale",
    "tremolo",
    "vibrato",
    "rotation",
    "distortion",
    "channelMix",
    "lowPass",
];

/// The fifteen equalizer bands, lowest first. The protocol names a band by its
/// index into this list.
pub const EQUALIZER_FREQUENCIES: [u32; 15] =
    [25, 40, 63, 100, 160, 250, 400, 630, 1000, 1600, 2500, 4000, 6300, 10000, 16000];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FilterError(pub String);

impl FilterError {
    fn new(message: impl Into<String>) -> Self {
        FilterError(message.into())
    }
}

/// A validated filter request.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FilterSet {
    /// The global volume and the volume filter combined.
    pub gain: f64,
    pub bands: Vec<EqBand>,
    pub karaoke: Option<Karaoke>,
    pub time: Option<Timescale>,
    pub tremolo: Option<Modulation>,
    pub vibrato: Option<Modulation>,
    pub rotation: Option<Rotation>,
    pub distort: Option<Distortion>,
    pub mix: Option<ChannelMix>,
    pub low_pass: Option<LowPass>,
}

/// One peaking band, already converted from the protocol's gain scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqBand {
    pub frequency: f64,
    pub decibels: f64,
}

/// Subtracts the centre channel, scaled by `gain`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Karaoke {
    pub filter_band: f64,
    pub filter_width: f64,
    /// Negative: the centre channel is taken away from the mix.
    pub gain: f64,
}

/// Changes the speed and the pitch of a track independently.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timescale {
    /// Changes tempo and keeps the pitch.
    pub speed: f64,
    /// Changes pitch and keeps the tempo.
    pub pitch: f64,
    /// Changes both.
    pub rate: f64,
}

impl Timescale {
    /// Whether the time scaling was asked for at all. A request that leaves all
    /// three controls at one asks for nothing, and running it would only add the
    /// delay the stretch introduces.
    pub fn changes_anything(&self) -> bool {
        self.speed != 1.0 || self.pitch != 1.0 || self.rate != 1.0
    }
}

/// The shared shape of tremolo and vibrato.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Modulation {
    pub frequency: f64,
    pub depth: f64,
}

/// Pans the two channels against each other.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rotation {
    pub hertz: f64,
}

/// The waveshaper the protocol exposes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Distortion {
    pub sin_offset: f64,
    pub sin_scale: f64,
    pub cos_offset: f64,
    pub cos_scale: f64,
    pub tan_offset: f64,
    pub tan_scale: f64,
    pub offset: f64,
    pub scale: f64,
}

/// The four-way stereo mixdown matrix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelMix {
    pub left_to_left: f64,
    pub left_to_right: f64,
    pub right_to_left: f64,
    pub right_to_right: f64,
}

/// A one pole low pass, held as the frequency it was derived from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LowPass {
    /// The protocol value.
    pub smoothing: f64,
    /// What the filter actually uses.
    pub frequency: f64,
}

impl FilterSet {
    /// The rate the track advances at, which the player uses to pace frames.
    pub fn tempo(&self) -> f64 {
        match self.time {
            Some(t) => t.speed * t.rate,
            None => 1.0,
        }
    }

    /// The filters a request actually asked for, in the order the graph applies
    /// them. A filter whose settings amount to doing nothing is left out, so the
    /// list is what has to be honoured rather than what was mentioned.
    pub fn active(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if !self.bands.is_empty() {
            names.push("equalizer");
        }
        if self.karaoke.is_some() {
            names.push("karaoke");
        }
        if self.time.is_some_and(|t| t.changes_anything()) {
            names.push("timescale");
        }
        if self.tremolo.is_some() {
            names.push("tremolo");
        }
        if self.vibrato.is_some() {
            names.push("vibrato");
        }
        if self.rotation.is_some() {
            names.push("rotation");
        }
        if self.distort.is_some() {
            names.push("distortion");
        }
        if self.mix.is_some() {
            names.push("channelMix");
        }
        if self.low_pass.is_some() {
            names.push("lowPass");
        }
        names
    }

    /// Renders the FFmpeg filter graph.
    ///
    /// The node order is fixed, so the same request always produces the same
    /// graph whatever order the fields arrived in.
    pub fn render_ffmpeg(&self) -> String {
        let mut parts: Vec<String> = vec![
            "aformat=sample_rates=48000:channel_layouts=stereo".into(),
            format!("volume={}", number(self.gain)),
        ];
        for band in &self.bands {
            parts.push(format!(
                "equalizer=f={}:t=o:w=0.666667:g={}",
                band.frequency as i64,
                number(band.decibels)
            ));
        }
        if let Some(k) = self.karaoke {
            parts.push(format!(
                "asplit=2[kdry][kmid];[kmid]pan=mono|c0=0.5*c0+0.5*c1,bandpass=f={}:t=h:w={},volume={},pan=stereo|c0=c0|c1=c0[kcut];[kdry][kcut]amix=inputs=2:normalize=0:duration=first",
                number(k.filter_band),
                number(k.filter_width),
                number(k.gain)
            ));
        }
        if let Some(t) = self.time {
            parts.push(format!("asetrate={}", number(48_000.0 * t.pitch * t.rate)));
            parts.push("aresample=48000".into());
            parts.extend(tempo_chain(t.speed / t.pitch));
        }
        if let Some(m) = self.tremolo {
            parts.push(format!("tremolo=f={}:d={}", number(m.frequency), number(m.depth)));
        }
        if let Some(m) = self.vibrato {
            parts.push(format!("vibrato=f={}:d={}", number(m.frequency), number(m.depth)));
        }
        if let Some(r) = self.rotation {
            let hz = number(r.hertz);
            parts.push(format!(
                "aeval=exprs='val(0)*(0.5+0.5*cos(2*PI*{hz}*t))|val(1)*(0.5-0.5*cos(2*PI*{hz}*t))'"
            ));
        }
        if let Some(d) = self.distort {
            let channel = |ch: usize| {
                let x = format!("val({ch})");
                format!(
                    "clip({}*(sin({x}*{}+{})*cos({x}*{}+{})*tan({x}*{}+{}))+{},-1,1)",
                    number(d.scale),
                    number(d.sin_scale),
                    number(d.sin_offset),
                    number(d.cos_scale),
                    number(d.cos_offset),
                    number(d.tan_scale),
                    number(d.tan_offset),
                    number(d.offset)
                )
            };
            parts.push(format!("aeval=exprs='{}|{}'", channel(0), channel(1)));
        }
        if let Some(m) = self.mix {
            parts.push(format!(
                "pan=stereo|c0={}*c0+{}*c1|c1={}*c0+{}*c1",
                number(m.left_to_left),
                number(m.right_to_left),
                number(m.left_to_right),
                number(m.right_to_right)
            ));
        }
        if let Some(l) = self.low_pass {
            parts.push(format!("lowpass=f={}:p=1", number(l.frequency)));
        }
        parts.join(",")
    }
}

/// Renders one tempo change as the sequence of `atempo` stages FFmpeg needs,
/// because a single stage only covers 0.5 to 2.
fn tempo_chain(mut value: f64) -> Vec<String> {
    let mut out = Vec::new();
    while value < 0.5 {
        out.push("atempo=0.5".to_string());
        value /= 0.5;
    }
    while value > 2.0 {
        out.push("atempo=2".to_string());
        value /= 2.0;
    }
    out.push(format!("atempo={}", number(value)));
    out
}

/// Formats a number the way the graph wants it: fixed point, eight decimals.
fn number(value: f64) -> String {
    format!("{value:.8}")
}

/// Reports disabled filters in the stable order of Rustlink's filter registry.
///
/// A filter that is absent from the configuration stays enabled, and unknown
/// filter names are left to [`parse`], which rejects them.
pub fn disabled_filters(
    filters: &Object,
    configured: &BTreeMap<String, bool>,
) -> Vec<&'static str> {
    FILTER_NAMES
        .iter()
        .filter(|name| filters.contains_key(**name) && configured.get(**name) == Some(&false))
        .copied()
        .collect()
}

/// Validates a filter request without building anything.
pub fn parse(filters: &Object, volume: i64) -> Result<FilterSet, FilterError> {
    if !(0..=1000).contains(&volume) {
        return Err(FilterError::new("volume must be 0..1000"));
    }
    for name in filters.keys() {
        if !FILTER_NAMES.contains(&name.as_str()) {
            return Err(FilterError::new(format!("unsupported filter: {name}")));
        }
    }

    let mut set = FilterSet { gain: volume as f64 / 100.0, ..Default::default() };
    if let Some(raw) = filters.get("volume") {
        let v = raw.as_f64().filter(|v| (0.0..=5.0).contains(v));
        match v {
            Some(v) => set.gain *= v,
            None => return Err(FilterError::new("filter volume must be 0..5")),
        }
    }
    if let Some(raw) = filters.get("equalizer") {
        set.bands = parse_equalizer(raw)?;
    }
    if let Some(raw) = filters.get("karaoke") {
        let v = fields(
            raw,
            &[
                Field::new("level", 1.0, 0.0, 1.0),
                Field::new("monoLevel", 1.0, 0.0, 1.0),
                Field::new("filterBand", 220.0, 1.0, 24_000.0),
                Field::new("filterWidth", 100.0, 1.0, 24_000.0),
            ],
        )?;
        set.karaoke = Some(Karaoke { filter_band: v[2], filter_width: v[3], gain: -v[0] * v[1] });
    }
    if let Some(raw) = filters.get("timescale") {
        let v = fields(
            raw,
            &[
                Field::new("speed", 1.0, 0.05, 5.0),
                Field::new("pitch", 1.0, 0.05, 5.0),
                Field::new("rate", 1.0, 0.05, 5.0),
            ],
        )?;
        set.time = Some(Timescale { speed: v[0], pitch: v[1], rate: v[2] });
    }
    for name in ["tremolo", "vibrato"] {
        let Some(raw) = filters.get(name) else { continue };
        let v = fields(
            raw,
            &[Field::new("frequency", 2.0, 0.1, 20.0), Field::new("depth", 0.5, 0.0, 1.0)],
        )?;
        let m = Modulation { frequency: v[0], depth: v[1] };
        if name == "tremolo" {
            set.tremolo = Some(m);
        } else {
            set.vibrato = Some(m);
        }
    }
    if let Some(raw) = filters.get("rotation") {
        let v = fields(raw, &[Field::new("rotationHz", 0.2, -20.0, 20.0)])?;
        if v[0] != 0.0 {
            set.rotation = Some(Rotation { hertz: v[0] });
        }
    }
    if let Some(raw) = filters.get("distortion") {
        let v = fields(
            raw,
            &[
                Field::new("sinOffset", 0.0, -10.0, 10.0),
                Field::new("sinScale", 1.0, -10.0, 10.0),
                Field::new("cosOffset", 0.0, -10.0, 10.0),
                Field::new("cosScale", 1.0, -10.0, 10.0),
                Field::new("tanOffset", 0.0, -10.0, 10.0),
                Field::new("tanScale", 1.0, -10.0, 10.0),
                Field::new("offset", 0.0, -10.0, 10.0),
                Field::new("scale", 1.0, -10.0, 10.0),
            ],
        )?;
        set.distort = Some(Distortion {
            sin_offset: v[0],
            sin_scale: v[1],
            cos_offset: v[2],
            cos_scale: v[3],
            tan_offset: v[4],
            tan_scale: v[5],
            offset: v[6],
            scale: v[7],
        });
    }
    if let Some(raw) = filters.get("channelMix") {
        let v = fields(
            raw,
            &[
                Field::new("leftToLeft", 1.0, 0.0, 1.0),
                Field::new("leftToRight", 0.0, 0.0, 1.0),
                Field::new("rightToLeft", 0.0, 0.0, 1.0),
                Field::new("rightToRight", 1.0, 0.0, 1.0),
            ],
        )?;
        set.mix = Some(ChannelMix {
            left_to_left: v[0],
            left_to_right: v[1],
            right_to_left: v[2],
            right_to_right: v[3],
        });
    }
    if let Some(raw) = filters.get("lowPass") {
        let v = fields(raw, &[Field::new("smoothing", 20.0, 1.0001, 1000.0)])?;
        set.low_pass = Some(LowPass {
            smoothing: v[0],
            frequency: 48_000.0 / (2.0 * std::f64::consts::PI * v[0]),
        });
    }
    Ok(set)
}

/// The equalizer is an array of bands rather than an object, so it is parsed on
/// its own.
fn parse_equalizer(raw: &Value) -> Result<Vec<EqBand>, FilterError> {
    let invalid = || FilterError::new("invalid equalizer");
    let Some(items) = raw.as_array() else { return Err(invalid()) };
    if items.len() > 15 {
        return Err(invalid());
    }
    let mut bands = Vec::with_capacity(items.len());
    let mut seen = [false; 15];
    for item in items {
        let Value::Object(map) = item else { return Err(invalid()) };
        let mut band = 0i64;
        let mut gain = 0.0f64;
        for (key, value) in map {
            match key.as_str() {
                "band" => band = value.as_i64().ok_or_else(invalid)?,
                "gain" => gain = value.as_f64().ok_or_else(invalid)?,
                _ => return Err(invalid()),
            }
        }
        // An omitted gain leaves the band flat.
        if !(0..=14).contains(&band) || !(-0.25..=1.0).contains(&gain) || seen[band as usize] {
            return Err(FilterError::new("invalid or duplicate equalizer band"));
        }
        seen[band as usize] = true;
        bands.push(EqBand {
            frequency: EQUALIZER_FREQUENCIES[band as usize] as f64,
            decibels: 20.0 * f64::max(0.0001, 1.0 + 4.0 * gain).log10(),
        });
    }
    Ok(bands)
}

/// One numeric parameter of a filter object.
struct Field {
    name: &'static str,
    default: f64,
    min: f64,
    max: f64,
}

impl Field {
    const fn new(name: &'static str, default: f64, min: f64, max: f64) -> Self {
        Field { name, default, min, max }
    }
}

/// Reads a filter object's numeric parameters, rejecting anything else.
///
/// The values come back in the order the fields were declared, so callers index
/// them positionally rather than through a map.
fn fields(raw: &Value, wanted: &[Field]) -> Result<Vec<f64>, FilterError> {
    let Some(map) = raw.as_object() else {
        return Err(FilterError::new("filter must be an object with numeric values"));
    };
    let mut out = Vec::with_capacity(wanted.len());
    for field in wanted {
        let value = match map.get(field.name) {
            Some(v) => v
                .as_f64()
                .ok_or_else(|| FilterError::new("filter must be an object with numeric values"))?,
            None => field.default,
        };
        if value.is_nan() || value.is_infinite() || value < field.min || value > field.max {
            return Err(FilterError::new(format!(
                "{} must be in [{}, {}]",
                field.name,
                bound(field.min),
                bound(field.max)
            )));
        }
        out.push(value);
    }
    if map.keys().any(|key| !wanted.iter().any(|f| f.name == key)) {
        return Err(FilterError::new("unknown filter parameter"));
    }
    Ok(out)
}

/// Formats a numeric bound without unnecessary decimal zeroes.
fn bound(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

impl fmt::Display for FilterSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render_ffmpeg())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: serde_json::Value) -> Object {
        match value {
            serde_json::Value::Object(m) => m,
            _ => panic!("not an object"),
        }
    }

    fn set(value: serde_json::Value) -> FilterSet {
        parse(&object(value), 100).unwrap()
    }

    fn error(value: serde_json::Value) -> String {
        parse(&object(value), 100).unwrap_err().0
    }

    #[test]
    fn an_empty_request_changes_nothing() {
        let filters = set(json!({}));
        assert_eq!(filters.gain, 1.0);
        assert_eq!(filters.tempo(), 1.0);
        assert!(filters.active().is_empty());
        assert_eq!(
            filters.render_ffmpeg(),
            "aformat=sample_rates=48000:channel_layouts=stereo,volume=1.00000000"
        );
    }

    #[test]
    fn the_player_volume_and_the_filter_multiply() {
        assert_eq!(parse(&object(json!({"volume": 0.5})), 80).unwrap().gain, 0.4);
    }

    #[test]
    fn impossible_values_are_refused_by_name() {
        assert_eq!(error(json!({"volume": 9})), "filter volume must be 0..5");
        assert_eq!(error(json!({"nope": {}})), "unsupported filter: nope");
        assert_eq!(error(json!({"tremolo": {"frequency": 100}})), "frequency must be in [0.1, 20]");
        assert_eq!(error(json!({"tremolo": {"wat": 1}})), "unknown filter parameter");
        assert_eq!(error(json!({"tremolo": 4})), "filter must be an object with numeric values");
        assert_eq!(parse(&object(json!({})), 1001).unwrap_err().0, "volume must be 0..1000");
    }

    #[test]
    fn unknown_filter_objects_are_rejected_even_when_empty() {
        assert_eq!(error(json!({"unknown_filter": {}})), "unsupported filter: unknown_filter");
    }

    #[test]
    fn equalizer_bands_convert_the_protocol_gain_scale() {
        let filters =
            set(json!({"equalizer": [{"band": 0, "gain": 0.0}, {"band": 14, "gain": 1.0}]}));
        assert_eq!(filters.bands.len(), 2);
        assert_eq!(filters.bands[0].frequency, 25.0);
        assert_eq!(filters.bands[0].decibels, 0.0);
        assert_eq!(filters.bands[1].frequency, 16_000.0);
        assert!((filters.bands[1].decibels - 13.979_400).abs() < 1e-4);
        assert!(filters.render_ffmpeg().contains("equalizer=f=25:t=o:w=0.666667:g=0.00000000"));
    }

    #[test]
    fn a_band_may_omit_its_gain_but_not_repeat_itself() {
        // Unspecified gain is neutral.
        let filters = set(json!({"equalizer": [{"band": 3}]}));
        assert_eq!(filters.bands[0].decibels, 0.0);
        let filters = set(json!({"equalizer": [{"band": 3, "gain": 0.0}]}));
        assert_eq!(filters.bands[0].decibels, 0.0);
        assert_eq!(
            error(json!({"equalizer": [{"band": 3}, {"band": 3}]})),
            "invalid or duplicate equalizer band"
        );
        assert_eq!(
            error(json!({"equalizer": [{"band": 15}]})),
            "invalid or duplicate equalizer band"
        );
        assert_eq!(
            error(json!({"equalizer": [{"band": 0, "gain": 2}]})),
            "invalid or duplicate equalizer band"
        );
        assert_eq!(error(json!({"equalizer": {"band": 0}})), "invalid equalizer");
    }

    #[test]
    fn timescale_splits_speed_from_pitch() {
        let filters = set(json!({"timescale": {"speed": 1.5}}));
        assert_eq!(filters.tempo(), 1.5);
        assert_eq!(filters.active(), vec!["timescale"]);
        assert!(filters.render_ffmpeg().contains("asetrate=48000.00000000"));
        assert!(filters.render_ffmpeg().contains("atempo=1.50000000"));

        let filters = set(json!({"timescale": {"speed": 1.5, "pitch": 1.5}}));
        assert_eq!(filters.tempo(), 1.5);
        assert!(filters.render_ffmpeg().contains("asetrate=72000.00000000"));
        assert!(filters.render_ffmpeg().contains("atempo=1.00000000"));
    }

    #[test]
    fn timescale_left_at_one_is_not_a_filter() {
        let filters = set(json!({"timescale": {"speed": 1.0, "pitch": 1.0, "rate": 1.0}}));
        assert!(filters.active().is_empty());
    }

    #[test]
    fn a_slow_tempo_becomes_several_stages() {
        let stages = tempo_chain(0.1);
        assert_eq!(stages, vec!["atempo=0.5", "atempo=0.5", "atempo=0.5", "atempo=0.80000000"]);
    }

    #[test]
    fn karaoke_subtracts_the_centre_channel() {
        let filters = set(json!({"karaoke": {"level": 0.5, "monoLevel": 1.0}}));
        let karaoke = filters.karaoke.unwrap();
        assert_eq!(karaoke.gain, -0.5);
        assert_eq!(karaoke.filter_band, 220.0);
        assert_eq!(karaoke.filter_width, 100.0);
    }

    #[test]
    fn a_rotation_of_zero_is_no_rotation() {
        assert!(set(json!({"rotation": {"rotationHz": 0}})).rotation.is_none());
        assert_eq!(set(json!({"rotation": {"rotationHz": 0.4}})).rotation.unwrap().hertz, 0.4);
    }

    #[test]
    fn low_pass_derives_its_frequency_from_the_smoothing() {
        let low_pass = set(json!({"lowPass": {"smoothing": 20}})).low_pass.unwrap();
        assert_eq!(low_pass.smoothing, 20.0);
        assert!((low_pass.frequency - 381.971_863).abs() < 1e-4);
    }

    #[test]
    fn channel_mix_and_distortion_render_their_own_graphs() {
        let filters = set(json!({"channelMix": {"leftToRight": 0.5}, "distortion": {"scale": 2}}));
        assert_eq!(filters.active(), vec!["distortion", "channelMix"]);
        let graph = filters.render_ffmpeg();
        assert!(graph
            .contains("pan=stereo|c0=1.00000000*c0+0.00000000*c1|c1=0.50000000*c0+1.00000000*c1"));
        assert!(graph.contains("aeval=exprs='clip(2.00000000*("));
    }

    #[test]
    fn the_graph_is_stable_whatever_order_the_fields_arrived_in() {
        let a = set(json!({"tremolo": {"frequency": 4}, "lowPass": {"smoothing": 30}}));
        let b = set(json!({"lowPass": {"smoothing": 30}, "tremolo": {"frequency": 4}}));
        assert_eq!(a.render_ffmpeg(), b.render_ffmpeg());
    }

    #[test]
    fn disabled_filters_are_reported_in_protocol_order() {
        let configured = BTreeMap::from([
            ("volume".to_string(), false),
            ("karaoke".to_string(), false),
            ("lowPass".to_string(), true),
        ]);
        let request = object(json!({"volume": 1, "karaoke": {}, "tremolo": {}}));
        assert_eq!(disabled_filters(&request, &configured), vec!["volume", "karaoke"]);
        assert!(disabled_filters(&object(json!({})), &configured).is_empty());
    }
}
