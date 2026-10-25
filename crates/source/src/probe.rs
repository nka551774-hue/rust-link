//! Reading a container's own header.
//!
//! A track loaded from a URL has no metadata until the node reads some out of
//! the bytes. Every format worth streaming states its duration and its tags in
//! the first few kilobytes, so the node reads those rather than handing the
//! stream to a separate probe process: a live stream is then identified without
//! waiting for any audio at all.

/// How much of a stream is read before giving up on finding a header.
pub const MAX_PROBE: usize = 8 << 20;

/// How much is read before deciding whether more is worth reading.
pub const FIRST_PROBE: usize = 16 << 10;

/// What the header said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    /// Whether a stream of audio was recognised at all.
    pub audio: bool,
    pub title: String,
    pub artist: String,
    pub artwork: Option<String>,
    /// The length in milliseconds, or zero when the header does not say.
    pub duration_ms: i64,
}

/// Container counters and HTTP lengths can exceed i64 during scaling even
/// when the resulting milliseconds fit. Unrepresentable durations stay unknown.
fn milliseconds(units: u128, rate: u64) -> i64 {
    if rate == 0 {
        return 0;
    }
    i64::try_from(units * 1000 / u128::from(rate)).unwrap_or(0)
}

/// Reads what the leading bytes say about a stream.
///
/// `size` is the whole length when the server declared one, and the probe uses
/// it to work out a duration for the formats that only state a bitrate.
pub fn probe(head: &[u8], size: i64) -> Probe {
    if head.len() < 4 {
        return Probe::default();
    }
    if head.starts_with(b"OggS") {
        return probe_ogg(head, size);
    }
    if head.starts_with(b"fLaC") {
        return probe_flac(head);
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE") {
        return probe_wav(head, size);
    }
    if head.starts_with(b"ID3") {
        return probe_mp3(head, size);
    }
    if head.get(4..8) == Some(b"ftyp") {
        return probe_mp4(head, size);
    }
    if head[0] == 0xff && head[1] & 0xf6 == 0xf0 {
        return probe_adts(head, size);
    }
    // A bare MPEG frame with no tag in front of it is still an MP3.
    if starts_mpeg_frame(head) {
        return probe_mp3(head, size);
    }
    if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        // Matroska states its tracks in a structure that is not worth walking
        // for the two fields the node needs; what matters is that it is a media
        // stream rather than a page of HTML.
        return Probe { audio: true, ..Probe::default() };
    }
    Probe::default()
}

/// Walks the top level boxes of an MP4, calling `visit` with each one's body.
///
/// A box is a length, a four character type and its body. A length of one means
/// the real length follows the type, which is how a box larger than four
/// gigabytes is written.
fn for_each_box<'a>(mut data: &'a [u8], mut visit: impl FnMut(&[u8; 4], &'a [u8])) {
    while data.len() >= 8 {
        let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&data[4..8]);
        let (body_start, body_len) = match size {
            0 => (8, data.len() - 8),
            1 => {
                if data.len() < 16 {
                    return;
                }
                let large = u64::from_be_bytes([
                    data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
                ]) as usize;
                (16, large.saturating_sub(16))
            }
            size => (8, size.saturating_sub(8)),
        };
        let body_end = (body_start + body_len).min(data.len());
        if body_end <= body_start {
            return;
        }
        visit(&kind, &data[body_start..body_end]);
        if body_end >= data.len() {
            return;
        }
        data = &data[body_end..];
    }
}

/// Finds a box by type among a set of boxes.
fn find_box<'a>(data: &'a [u8], want: &[u8; 4]) -> Option<&'a [u8]> {
    let mut found = None;
    for_each_box(data, |kind, body| {
        if kind == want && found.is_none() {
            found = Some(body);
        }
    });
    found
}

fn probe_mp4(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    // A stream that has not been indexed yet has no movie box in its head, which
    // is what a live broadcast looks like.
    let Some(moov) = find_box(head, b"moov") else {
        return out;
    };
    out.audio = moov_has_audio(moov);
    if let Some(mvhd) = find_box(moov, b"mvhd") {
        if let Some((timescale, duration)) = read_mvhd(mvhd) {
            if timescale > 0 {
                out.duration_ms = milliseconds(duration as u128, timescale as u64);
            }
        }
    }
    read_mp4_tags(moov, &mut out);
    if out.duration_ms == 0 && size > 0 && !out.audio {
        out.duration_ms = 0;
    }
    out
}

fn moov_has_audio(moov: &[u8]) -> bool {
    let mut audio = false;
    for_each_box(moov, |kind, body| {
        if kind != b"trak" {
            return;
        }
        if let Some(handler) = find_box(body, b"mdia").and_then(|mdia| find_box(mdia, b"hdlr")) {
            // The handler type sits after a version, some flags and a
            // pre-defined field.
            if handler.len() >= 12 && &handler[8..12] == b"soun" {
                audio = true;
            }
        }
    });
    audio
}

/// Reads the timescale and duration out of a movie header.
fn read_mvhd(mvhd: &[u8]) -> Option<(u32, u64)> {
    let version = *mvhd.first()?;
    if version == 1 {
        // Version one widens the creation and modification times to 64 bits,
        // which pushes the timescale to byte 20 and the duration to 24.
        let timescale = u32::from_be_bytes(mvhd.get(20..24)?.try_into().ok()?);
        let duration = u64::from_be_bytes(mvhd.get(24..32)?.try_into().ok()?);
        (duration != u64::MAX).then_some((timescale, duration))
    } else if version == 0 {
        let timescale = u32::from_be_bytes(mvhd.get(12..16)?.try_into().ok()?);
        let duration = u32::from_be_bytes(mvhd.get(16..20)?.try_into().ok()?) as u64;
        (duration != u32::MAX as u64).then_some((timescale, duration))
    } else {
        None
    }
}

/// The four character tags worth reading, and which field each one fills.
const MP4_TAGS: [([u8; 4], &str); 3] =
    [(*b"\xa9nam", "title"), (*b"\xa9ART", "artist"), (*b"covr", "artwork")];

fn read_mp4_tags(moov: &[u8], out: &mut Probe) {
    let Some(ilst) = find_box(moov, b"udta")
        .and_then(|udta| find_box(udta, b"meta"))
        .and_then(|meta| {
            // QuickTime permits a meta atom with a leading handler child and
            // no FullBox prefix; ISO MP4 puts four version/flag bytes first.
            if meta.get(4..8) == Some(b"hdlr") {
                Some(meta)
            } else {
                meta.get(4..)
            }
        })
        .and_then(|children| find_box(children, b"ilst"))
    else {
        return;
    };
    for_each_box(ilst, |kind, body| {
        let Some((_, field)) = MP4_TAGS.iter().find(|(tag, _)| tag == kind) else {
            return;
        };
        let Some(data) = find_box(body, b"data") else {
            return;
        };
        // A data box is a type, a locale, then the value itself.
        let Some(value) = data.get(8..) else {
            return;
        };
        match *field {
            "title" => out.title = decode_text(value),
            "artist" => out.artist = decode_text(value),
            // Artwork is a picture rather than text, and the node does not carry
            // one out of a file it is about to stream.
            _ => {}
        }
    });
}

/// Decodes a tag value that may be UTF-8 or UTF-16.
fn decode_text(value: &[u8]) -> String {
    // A UTF-16 value starts with a byte order mark, which is what tells it apart
    // from UTF-8 that happens to hold zero bytes.
    if value.len() >= 2 && value[0] == 0xff && value[1] == 0xfe {
        let units: Vec<u16> =
            value[2..].as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        return String::from_utf16_lossy(&units).trim().to_string();
    }
    if value.len() >= 2 && value[0] == 0xfe && value[1] == 0xff {
        let units: Vec<u16> =
            value[2..].as_chunks::<2>().0.iter().map(|pair| u16::from_be_bytes(*pair)).collect();
        return String::from_utf16_lossy(&units).trim().to_string();
    }
    String::from_utf8_lossy(value).trim().to_string()
}

fn probe_flac(head: &[u8]) -> Probe {
    let mut out = Probe { audio: true, ..Probe::default() };
    let mut at = 4;
    while let Some(header) = head.get(at..at + 4) {
        let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        let Some(block) = head.get(at + 4..at + 4 + length) else {
            break;
        };
        match header[0] & 0x7f {
            0 if block.len() == 34 => {
                let packed = u64::from_be_bytes(block[10..18].try_into().unwrap());
                let rate = (packed >> 44) & 0x000f_ffff;
                let total = packed & 0x0000_000f_ffff_ffff;
                out.duration_ms = milliseconds(total as u128, rate);
            }
            4 => read_vorbis_comment(block, &mut out),
            _ => {}
        }
        if header[0] & 0x80 != 0 {
            break;
        }
        at += 4 + length;
    }
    out
}

/// Reads the field names a Vorbis comment block holds.
fn read_vorbis_comment(data: &[u8], out: &mut Probe) {
    if data.len() < 8 {
        return;
    }
    let vendor = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut at = 4 + vendor;
    let Some(count) =
        data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    else {
        return;
    };
    at += 4;
    for _ in 0..count.min(1024) {
        let Some(length) =
            data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        else {
            return;
        };
        at += 4;
        let Some(field) = data.get(at..at + length) else {
            return;
        };
        at += length;
        let field = String::from_utf8_lossy(field);
        let Some((name, value)) = field.split_once('=') else {
            continue;
        };
        match name.to_ascii_uppercase().as_str() {
            "TITLE" => out.title = value.trim().to_string(),
            "ARTIST" => out.artist = value.trim().to_string(),
            _ => {}
        }
    }
}

fn probe_ogg(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    // The first page's payload names the codec and carries the rate.
    let segments = head.get(26).copied().unwrap_or(0) as usize;
    let payload = head.get(27 + segments..).unwrap_or_default();
    let mut rate = 0i64;
    if payload.starts_with(b"OpusHead") {
        out.audio = true;
        // Opus is always decoded at 48 kHz.
        rate = 48_000;
    } else if payload.starts_with(&[0x01, b'v', b'o', b'r', b'b', b'i', b's']) {
        out.audio = true;
        // The rate is a little endian integer after the version and the channel
        // count.
        if let Some(bytes) = payload.get(12..16) {
            rate = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64;
        }
    } else if payload.starts_with(b"\x7fFLAC") {
        out.audio = true;
    }
    // The length of an Ogg stream is only stated by the granule of its last
    // page, so a duration needs the tail. A stream with an unknown length is
    // reported as live rather than guessed at.
    if rate > 0 && size > 0 && head.len() as i64 == size {
        if let Some(granule) = last_granule(head) {
            if granule > 0 {
                let skip = if payload.starts_with(b"OpusHead") {
                    payload
                        .get(10..12)
                        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]) as i64)
                        .unwrap_or(0)
                } else {
                    0
                };
                out.duration_ms =
                    milliseconds(granule.saturating_sub(skip).max(0) as u128, rate as u64);
            }
        }
    }
    out
}

/// Reads an end-of-stream granule only when its complete page is present.
fn last_granule(data: &[u8]) -> Option<i64> {
    let mut at = 0usize;
    let mut granule = None;
    while at + 27 <= data.len() {
        if &data[at..at + 4] != b"OggS" {
            break;
        }
        let segments = data[at + 26] as usize;
        let header = 27 + segments;
        let Some(table) = data.get(at + 27..at + 27 + segments) else {
            break;
        };
        let body: usize = table.iter().map(|count| *count as usize).sum();
        if at + header + body > data.len() {
            break;
        }
        let bytes = data.get(at + 6..at + 14)?;
        granule = (data[at + 5] & 4 != 0).then(|| i64::from_le_bytes(bytes.try_into().unwrap()));
        at += header + body;
    }
    granule
}

fn probe_wav(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    let mut rate = 0u32;
    let mut channels = 0u32;
    let mut bits = 0u32;
    let mut data_len = 0i64;
    let mut at = 12usize;
    while at + 8 <= head.len() {
        let kind = &head[at..at + 4];
        let length =
            u32::from_le_bytes([head[at + 4], head[at + 5], head[at + 6], head[at + 7]]) as usize;
        let body = head.get(at + 8..at + 8 + length).unwrap_or_default();
        match kind {
            b"fmt " => {
                out.audio = true;
                if body.len() >= 16 {
                    channels = u16::from_le_bytes([body[2], body[3]]) as u32;
                    rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                    bits = u16::from_le_bytes([body[14], body[15]]) as u32;
                }
            }
            b"data" => {
                out.audio = true;
                data_len = length as i64;
            }
            b"LIST" => read_riff_info(body, &mut out),
            _ => {}
        }
        // A chunk is padded to an even length.
        at += 8 + length + (length & 1);
    }
    if rate > 0 && channels > 0 && bits > 0 {
        let bytes_per_frame = (channels * bits / 8) as i64;
        let frames = if data_len > 0 { data_len } else { size.saturating_sub(44) };
        if bytes_per_frame > 0 && frames > 0 {
            out.duration_ms = milliseconds(frames as u128, bytes_per_frame as u64 * rate as u64);
        }
    }
    out
}

fn read_riff_info(body: &[u8], out: &mut Probe) {
    if !body.starts_with(b"INFO") {
        return;
    }
    let mut at = 4usize;
    while at + 8 <= body.len() {
        let kind = &body[at..at + 4];
        let length =
            u32::from_le_bytes([body[at + 4], body[at + 5], body[at + 6], body[at + 7]]) as usize;
        let value = body.get(at + 8..at + 8 + length).unwrap_or_default();
        match kind {
            b"INAM" => {
                out.title = String::from_utf8_lossy(value).trim_matches('\0').trim().to_string()
            }
            b"IART" => {
                out.artist = String::from_utf8_lossy(value).trim_matches('\0').trim().to_string()
            }
            _ => {}
        }
        at += 8 + length + (length & 1);
    }
}

fn starts_mpeg_frame(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xff && data[1] & 0xe0 == 0xe0 && data[1] & 0x06 != 0
}

fn probe_mp3(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    let mut audio_start = 0usize;
    if head.starts_with(b"ID3") && head.len() >= 10 {
        let length = synchsafe(&head[6..10]);
        // The footer, when there is one, is another ten bytes.
        let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
        read_id3(&head[10..(10 + length).min(head.len())], &mut out);
        audio_start = 10 + length + footer;
    }
    let Some(frame) = head.get(audio_start..).filter(|rest| starts_mpeg_frame(rest)) else {
        // A tag with no frame after it is still an MP3 with metadata, which is
        // all a stream needs to be described.
        out.audio = out.audio || !out.title.is_empty() || head.starts_with(b"ID3");
        return out;
    };
    out.audio = true;
    if let Some((bitrate, rate)) = parse_mpeg_frame(frame) {
        if bitrate > 0 && size > audio_start as i64 {
            out.duration_ms = milliseconds((size - audio_start as i64) as u128 * 8, bitrate as u64);
        }
        let _ = rate;
    }
    out
}

/// Reads the frames of an ID3v2 tag that name the track.
fn read_id3(data: &[u8], out: &mut Probe) {
    let mut at = 0usize;
    while at + 10 <= data.len() {
        let id = &data[at..at + 4];
        if !id.iter().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()) {
            return;
        }
        let length =
            u32::from_be_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]) as usize;
        let Some(body) = data.get(at + 10..at + 10 + length) else {
            return;
        };
        // The frame body opens with the encoding of the text it carries.
        let text = decode_id3_text(body);
        match id {
            b"TIT2" => out.title = text,
            b"TPE1" => out.artist = text,
            _ => {}
        }
        at += 10 + length;
    }
}

fn decode_id3_text(body: &[u8]) -> String {
    let Some(encoding) = body.first() else {
        return String::new();
    };
    let value = &body[1..];
    match encoding {
        // UTF-16 with a byte order mark, little or big endian.
        1 => decode_text(value),
        2 => {
            let units: Vec<u16> =
                value.as_chunks::<2>().0.iter().map(|pair| u16::from_be_bytes(*pair)).collect();
            String::from_utf16_lossy(&units).trim_matches('\0').trim().to_string()
        }
        3 => String::from_utf8_lossy(value).trim_matches('\0').trim().to_string(),
        // Latin-1, which is what a byte is.
        _ => value
            .iter()
            .map(|byte| *byte as char)
            .collect::<String>()
            .trim_matches('\0')
            .trim()
            .to_string(),
    }
}

/// Reads the bitrate and rate out of an MPEG audio frame header.
fn parse_mpeg_frame(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 4 || !starts_mpeg_frame(data) {
        return None;
    }
    let header = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let version = (header >> 19) & 0x3;
    let layer = (header >> 17) & 0x3;
    let bitrate_index = ((header >> 12) & 0xf) as usize;
    let rate_index = ((header >> 10) & 0x3) as usize;
    // Version one, layer three, is the table that matters; the others are rare
    // enough that a duration is not worth the extra tables.
    let rates = [44_100u32, 48_000, 32_000];
    let bitrates = [0u32, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];
    if version != 0x3
        || layer != 0x1
        || rate_index >= rates.len()
        || bitrate_index >= bitrates.len()
    {
        return None;
    }
    Some((bitrates[bitrate_index] * 1000, rates[rate_index]))
}

fn synchsafe(data: &[u8]) -> usize {
    data.iter().fold(0usize, |value, byte| (value << 7) | (*byte as usize & 0x7f))
}

/// The sample rates an ADTS header can name, by index.
const ADTS_RATES: [u32; 16] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350, 0, 0, 0,
];

fn probe_adts(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    if head.len() < 7 {
        return out;
    }
    let rate_index = ((head[2] >> 2) & 0xf) as usize;
    let rate = ADTS_RATES.get(rate_index).copied().unwrap_or(0);
    if rate == 0 {
        return out;
    }
    out.audio = true;
    // An ADTS stream states no duration, but the frames are a fixed number of
    // samples, so the length follows from the file size and the average bitrate
    // the header implies.
    let channels = ((head[2] & 0x1) << 2) | (head[3] >> 6);
    if size > 0 && channels > 0 {
        // 1024 samples a frame at the rate, and the frame length is stated in
        // every header; the first one is a fair average.
        let frame_len =
            (((head[3] as i64) & 0x3) << 11) | ((head[4] as i64) << 3) | ((head[5] as i64) >> 5);
        if frame_len > 0 {
            let frames = size / frame_len;
            out.duration_ms = milliseconds(frames as u128 * 1024, rate as u64);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a WAV header with a data chunk of the given length.
    fn wav(rate: u32, channels: u16, bits: u16, data_len: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * (channels * bits / 8) as u32).to_le_bytes());
        out.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out
    }

    #[test]
    fn a_wav_states_its_length_and_its_rate() {
        let head = wav(48_000, 2, 16, 48_000 * 4);
        let probe = probe(&head, head.len() as i64);
        assert!(probe.audio);
        assert_eq!(probe.duration_ms, 1000);
    }

    #[test]
    fn a_wav_keeps_the_names_it_was_given() {
        let mut head = wav(44_100, 1, 16, 44_100 * 2);
        // A writer puts the names in a chunk of their own, ahead of the audio.
        let data = head.split_off(36);
        let info = b"INFOINAM\x04\0\0\0nameIART\x06\0\0\0artist";
        head.extend_from_slice(b"LIST");
        head.extend_from_slice(&(info.len() as u32).to_le_bytes());
        head.extend_from_slice(info);
        head.extend_from_slice(&data);
        let probe = probe(&head, head.len() as i64);
        assert_eq!(probe.title, "name");
        assert_eq!(probe.artist, "artist");
    }

    #[test]
    fn an_id3_tag_is_read_before_the_frames() {
        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3\x03\x00\x00");
        tag.extend_from_slice(&[0, 0, 0, 17]);
        // The frame header is the id, the length and two flag bytes.
        tag.extend_from_slice(b"TIT2\x00\x00\x00\x07\x00\x00\x00a song");
        let probe = probe(&tag, tag.len() as i64);
        assert_eq!(probe.title, "a song");
    }

    #[test]
    fn an_mp3_duration_follows_from_its_bitrate() {
        // 128 kbit/s, 44.1 kHz, stereo, no padding.
        let mut head = vec![0xff, 0xfb, 0x90, 0x00];
        head.resize(64 * 1024, 0);
        let probe = probe(&head, head.len() as i64);
        assert!(probe.audio);
        // 64 KiB at 128 kbit/s is about four seconds.
        assert!((3900..4100).contains(&probe.duration_ms), "{}", probe.duration_ms);
    }

    #[test]
    fn a_flac_states_its_length_in_its_stream_info() {
        let mut head = Vec::new();
        head.extend_from_slice(b"fLaC");
        head.push(0x00);
        head.extend_from_slice(&[0, 0, 34]);
        head.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
        head.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        // Rate 44100 and 88200 samples, packed into the last eight bytes.
        let packed: u64 = (44_100u64 << 44) | 88_200;
        head.extend_from_slice(&packed.to_be_bytes());
        head.extend_from_slice(&[0; 16]);
        let probe = probe(&head, head.len() as i64);
        assert!(probe.audio);
        assert_eq!(probe.duration_ms, 2000);
    }

    #[test]
    fn a_vorbis_comment_names_the_track() {
        let mut block = Vec::new();
        let vendor = b"rustlink";
        block.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        block.extend_from_slice(vendor);
        block.extend_from_slice(&2u32.to_le_bytes());
        for field in ["TITLE=a song", "ARTIST=someone"] {
            block.extend_from_slice(&(field.len() as u32).to_le_bytes());
            block.extend_from_slice(field.as_bytes());
        }
        let mut out = Probe::default();
        read_vorbis_comment(&block, &mut out);
        assert_eq!(out.title, "a song");
        assert_eq!(out.artist, "someone");
    }

    #[test]
    fn an_ogg_page_states_the_granule_it_ends_at() {
        let mut page = Vec::new();
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(4);
        page.extend_from_slice(&48_000i64.to_le_bytes());
        page.extend_from_slice(&[1, 2, 3, 4]);
        page.extend_from_slice(&[0u8; 8]);
        page.push(1);
        page.push(255);
        page.resize(27 + 1 + 255, 0);
        assert_eq!(last_granule(&page), Some(48_000));
    }

    #[test]
    fn an_ogg_opus_stream_is_recognised() {
        let mut head = Vec::new();
        head.extend_from_slice(b"OggS");
        head.extend_from_slice(&[0u8; 22]);
        head.push(1);
        head.push(19);
        head.extend_from_slice(b"OpusHead");
        head.extend_from_slice(&[1, 2, 0, 0, 0x80, 0xbb, 0, 0, 0, 0, 0]);
        let probe = probe(&head, 0);
        assert!(probe.audio);
    }

    #[test]
    fn bytes_that_are_not_media_are_not_audio() {
        let probe = probe(b"<html><body>not audio</body></html>", 100);
        assert!(!probe.audio);
    }

    #[test]
    fn an_mp4_states_its_length_in_its_movie_header() {
        let mut mvhd = vec![0u8];
        mvhd.extend_from_slice(&[0u8; 3]);
        mvhd.extend_from_slice(&[0u8; 8]);
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&30_000u32.to_be_bytes());
        mvhd.extend_from_slice(&[0u8; 80]);
        let mut moov = Vec::new();
        moov.extend_from_slice(&((mvhd.len() + 8) as u32).to_be_bytes());
        moov.extend_from_slice(b"mvhd");
        moov.extend_from_slice(&mvhd);
        let mut head = Vec::new();
        head.extend_from_slice(&[0, 0, 0, 16]);
        head.extend_from_slice(b"ftypM4A ");
        head.extend_from_slice(&[0u8; 4]);
        head.extend_from_slice(&((moov.len() + 8) as u32).to_be_bytes());
        head.extend_from_slice(b"moov");
        head.extend_from_slice(&moov);
        let probe = probe(&head, head.len() as i64);
        assert_eq!(probe.duration_ms, 30_000);
    }
}
