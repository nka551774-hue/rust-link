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

/// Whether the head is a container whose index may sit further on, so that a
/// file which says nothing yet is worth reading on. An MP4 or M4A written
/// without "fast start" keeps its `moov` box after the audio data.
pub fn may_index_later(head: &[u8]) -> bool {
    head.get(4..8) == Some(b"ftyp")
}

/// Whether reading past `head` (up to `MAX_PROBE`) could yield metadata or a
/// duration that `head` does not yet provide.
///
/// Formats whose probe does not inspect further bytes (Matroska/WebM, ADTS, or
/// an Ogg larger than `MAX_PROBE`) stop after the initial 16 KiB read instead
/// of downloading 8 MiB of audio only to discard it.
pub fn can_gain_from_more(head: &[u8], size: i64) -> bool {
    if size <= 0 || head.len() as i64 >= size {
        return false;
    }
    if head.get(4..8) == Some(b"ftyp") {
        return find_box(head, b"moov").is_none();
    }
    if head.starts_with(b"OggS") {
        return size <= MAX_PROBE as i64;
    }
    if head.starts_with(b"fLaC") {
        let mut at = 4usize;
        while let Some(header) = head.get(at..at + 4) {
            let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
            let Some(next) = at.checked_add(4).and_then(|n| n.checked_add(length)) else {
                return false;
            };
            if next > head.len() {
                return true;
            }
            if header[0] & 0x80 != 0 {
                return false;
            }
            at = next;
        }
        return head.len() < 8;
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE") {
        let mut saw_fmt = false;
        let mut saw_data = false;
        let mut at = 12usize;
        while at + 8 <= head.len() {
            let kind = &head[at..at + 4];
            let length =
                u32::from_le_bytes([head[at + 4], head[at + 5], head[at + 6], head[at + 7]])
                    as usize;
            if kind == b"fmt " {
                saw_fmt = head.len() >= at + 24;
            } else if kind == b"data" {
                saw_data = true;
            }
            if saw_fmt && saw_data {
                return false;
            }
            let Some(next) = at
                .checked_add(8)
                .and_then(|n| n.checked_add(length))
                .and_then(|n| n.checked_add(length & 1))
            else {
                return false;
            };
            if next > head.len() {
                return !(saw_fmt && saw_data);
            }
            at = next;
        }
        return !(saw_fmt && saw_data);
    }
    if head.starts_with(b"ID3") && head.len() >= 10 {
        let length = synchsafe(&head[6..10]);
        let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
        return head.len()
            < 10usize.saturating_add(length).saturating_add(footer).saturating_add(64);
    }
    false
}

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
                ]);
                if large < 16 {
                    return;
                }
                let Ok(large) = usize::try_from(large) else {
                    return;
                };
                (16, large - 16)
            }
            2..=7 => return,
            size => (8, size - 8),
        };
        let available = data.len() - body_start;
        let truncated = body_len > available;
        let take = body_len.min(available);
        visit(&kind, &data[body_start..body_start + take]);
        if truncated || body_start + take >= data.len() {
            return;
        }
        data = &data[body_start + take..];
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
    let Some(after_vendor) = 4usize.checked_add(vendor) else {
        return;
    };
    let Some(count_end) = after_vendor.checked_add(4) else {
        return;
    };
    let Some(count) = data
        .get(after_vendor..count_end)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    else {
        return;
    };
    let mut at = count_end;
    for _ in 0..count.min(1024) {
        let Some(len_end) = at.checked_add(4) else {
            return;
        };
        let Some(length) =
            data.get(at..len_end).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        else {
            return;
        };
        let Some(field_end) = len_end.checked_add(length) else {
            return;
        };
        let Some(field) = data.get(len_end..field_end) else {
            return;
        };
        at = field_end;
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
    let mut avg_bytes_per_sec = 0u32;
    let mut data_len = 0i64;
    let mut streaming_data = false;
    let mut at = 12usize;
    while at + 8 <= head.len() {
        let kind = &head[at..at + 4];
        let raw_len = u32::from_le_bytes([head[at + 4], head[at + 5], head[at + 6], head[at + 7]]);
        let length = raw_len as usize;
        let body_end = at.saturating_add(8).saturating_add(length).min(head.len());
        let body = head.get(at + 8..body_end).unwrap_or_default();
        match kind {
            b"fmt " => {
                out.audio = true;
                if body.len() >= 16 {
                    channels = u16::from_le_bytes([body[2], body[3]]) as u32;
                    rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                    avg_bytes_per_sec = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                    bits = u16::from_le_bytes([body[14], body[15]]) as u32;
                }
            }
            b"data" => {
                out.audio = true;
                if raw_len == u32::MAX {
                    streaming_data = true;
                } else {
                    data_len = length as i64;
                }
            }
            b"LIST" => read_riff_info(body, &mut out),
            _ => {}
        }
        // A chunk is padded to an even length; stop if the chunk runs past `head`.
        let Some(next) = at
            .checked_add(8)
            .and_then(|n| n.checked_add(length))
            .and_then(|n| n.checked_add(length & 1))
        else {
            break;
        };
        if next > head.len() {
            break;
        }
        at = next;
    }
    let bytes_per_sec =
        if rate > 0 && channels > 0 && bits > 0 && (channels * bits).is_multiple_of(8) {
            u64::from(channels * bits / 8) * u64::from(rate)
        } else {
            u64::from(avg_bytes_per_sec)
        };
    let payload_bytes = if data_len > 0 {
        data_len
    } else if streaming_data && size <= 0 {
        0
    } else {
        size.saturating_sub(44)
    };
    if bytes_per_sec > 0 && payload_bytes > 0 {
        out.duration_ms = milliseconds(payload_bytes as u128, bytes_per_sec);
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

fn valid_id3_header(head: &[u8]) -> bool {
    head.len() >= 10
        && head.starts_with(b"ID3")
        && head[3] < 0xff
        && head[4] < 0xff
        && head[6..10].iter().all(|byte| byte & 0x80 == 0)
}

fn probe_mp3(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    let mut audio_start = 0usize;
    let mut saw_id3 = false;
    if valid_id3_header(head) {
        saw_id3 = true;
        let flags = head[5];
        let length = synchsafe(&head[6..10]);
        // The footer, when there is one, is another ten bytes.
        let footer = if flags & 0x10 != 0 { 10 } else { 0 };
        let tag_end = 10usize.saturating_add(length).min(head.len());
        read_id3(&head[10..tag_end], head[3], flags, &mut out);
        audio_start = 10usize.saturating_add(length).saturating_add(footer);
    }
    let Some(frame) = head.get(audio_start..).and_then(find_mpeg_sync) else {
        // A tag with no frame after it is still an MP3 with metadata, which is
        // all a stream needs to be described.
        out.audio = out.audio || !out.title.is_empty() || saw_id3;
        return out;
    };
    out.audio = true;
    if let Some(mpeg) = parse_mpeg_frame(frame) {
        if let Some(frames) = parse_vbr_frames(frame, &mpeg) {
            out.duration_ms = milliseconds(
                u128::from(frames) * u128::from(mpeg.samples_per_frame),
                u64::from(mpeg.rate),
            );
        } else if mpeg.bitrate > 0 && size > audio_start as i64 {
            out.duration_ms =
                milliseconds((size - audio_start as i64) as u128 * 8, u64::from(mpeg.bitrate));
        }
    }
    out
}

fn find_mpeg_sync(data: &[u8]) -> Option<&[u8]> {
    let limit = data.len().min(4096);
    for offset in 0..limit {
        if parse_mpeg_frame(&data[offset..]).is_some() {
            return Some(&data[offset..]);
        }
    }
    None
}

/// Reads the frames of an ID3v2 tag that name the track.
fn read_id3(data: &[u8], version: u8, flags: u8, out: &mut Probe) {
    let mut at = 0usize;
    if flags & 0x40 != 0 {
        let Some(ext_hdr) = data.get(..4) else {
            return;
        };
        let ext_len = if version == 4 {
            synchsafe(ext_hdr)
        } else {
            4usize.saturating_add(u32::from_be_bytes([
                ext_hdr[0], ext_hdr[1], ext_hdr[2], ext_hdr[3],
            ]) as usize)
        };
        if ext_len < 4 || ext_len > data.len() {
            return;
        }
        at = ext_len;
    }
    while at + 10 <= data.len() {
        let id = &data[at..at + 4];
        if !id.iter().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()) {
            return;
        }
        // ID3v2.4 uses seven bits per byte for frame sizes, just like the
        // outer tag. ID3v2.3 frames retain their ordinary big-endian size.
        let length = if version == 4 {
            synchsafe(&data[at + 4..at + 8])
        } else {
            u32::from_be_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]) as usize
        };
        let Some(body_end) = at.checked_add(10).and_then(|n| n.checked_add(length)) else {
            return;
        };
        let Some(body) = data.get(at + 10..body_end) else {
            return;
        };
        // The frame body opens with the encoding of the text it carries.
        let text = decode_id3_text(body);
        match id {
            b"TIT2" => out.title = text,
            b"TPE1" => out.artist = text,
            _ => {}
        }
        at = body_end;
    }
}

fn decode_id3_text(body: &[u8]) -> String {
    let Some(encoding) = body.first() else {
        return String::new();
    };
    let value = &body[1..];
    match encoding {
        // UTF-16 with a byte order mark, little or big endian.
        1 => decode_text(value).trim_matches('\0').trim().to_string(),
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

struct MpegFrame {
    bitrate: u32,
    rate: u32,
    samples_per_frame: u32,
    side_info_len: usize,
}

/// Reads the bitrate, sample rate and VBR header offset out of an MPEG frame.
fn parse_mpeg_frame(data: &[u8]) -> Option<MpegFrame> {
    if data.len() < 4 || !starts_mpeg_frame(data) {
        return None;
    }
    let header = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let version = (header >> 19) & 0x3;
    let layer = (header >> 17) & 0x3;
    let bitrate_index = ((header >> 12) & 0xf) as usize;
    let rate_index = ((header >> 10) & 0x3) as usize;
    let channel_mode = (header >> 6) & 0x3;
    if version == 0x1
        || layer == 0x0
        || rate_index >= 3
        || bitrate_index == 0
        || bitrate_index == 15
    {
        return None;
    }
    let base_rates = [44_100u32, 48_000, 32_000];
    let rate = match version {
        0x3 => base_rates[rate_index],
        0x2 => base_rates[rate_index] / 2,
        0x0 => base_rates[rate_index] / 4,
        _ => return None,
    };
    let kbps =
        match (version, layer) {
            (0x3, 0x1) => {
                [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0][bitrate_index]
            }
            (0x3, 0x2) => [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0]
                [bitrate_index],
            (0x3, 0x3) => [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0]
                [bitrate_index],
            (_, 0x3) => [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0]
                [bitrate_index],
            _ => [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0][bitrate_index],
        };
    let samples_per_frame = match (version, layer) {
        (_, 0x3) => 384,
        (0x3, _) => 1152,
        (_, 0x2) => 1152,
        _ => 576,
    };
    let mono = channel_mode == 0x3;
    let side_info_len = match (version == 0x3, mono) {
        (true, false) => 32,
        (true, true) | (false, false) => 17,
        (false, true) => 9,
    };
    Some(MpegFrame { bitrate: kbps * 1000, rate, samples_per_frame, side_info_len })
}

fn parse_vbr_frames(frame: &[u8], mpeg: &MpegFrame) -> Option<u32> {
    let xing_at = 4 + mpeg.side_info_len;
    if let Some(tag) = frame.get(xing_at..xing_at + 12) {
        if (&tag[..4] == b"Xing" || &tag[..4] == b"Info") && (tag[7] & 0x01 != 0) {
            let frames = u32::from_be_bytes([tag[8], tag[9], tag[10], tag[11]]);
            if frames > 0 {
                return Some(frames);
            }
        }
    }
    if let Some(vbri) = frame.get(36..54) {
        if &vbri[..4] == b"VBRI" {
            let frames = u32::from_be_bytes([vbri[14], vbri[15], vbri[16], vbri[17]]);
            if frames > 0 {
                return Some(frames);
            }
        }
    }
    None
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
        // Empty 8-byte `wide` / `free` boxes often sit between `ftyp` and `moov`.
        head.extend_from_slice(&[0, 0, 0, 8]);
        head.extend_from_slice(b"wide");
        head.extend_from_slice(&((moov.len() + 8) as u32).to_be_bytes());
        head.extend_from_slice(b"moov");
        head.extend_from_slice(&moov);
        let probe = probe(&head, head.len() as i64);
        assert_eq!(probe.duration_ms, 30_000);
    }

    #[test]
    fn vbr_mp3_uses_xing_frame_count_instead_of_header_frame_bitrate() {
        // 32 kbit/s silent Xing header frame followed by a 256 kbit/s body.
        let mut head = vec![0xff, 0xfb, 0x10, 0x00];
        head.resize(36, 0);
        head.extend_from_slice(b"Xing\x00\x00\x00\x01");
        head.extend_from_slice(&3828u32.to_be_bytes()); // ~100 seconds at 44.1 kHz
        head.resize(1024, 0);
        let probe = probe(&head, 3_200_000);
        assert!(probe.audio);
        assert!((99_900..=100_100).contains(&probe.duration_ms), "{}", probe.duration_ms);
    }
}
