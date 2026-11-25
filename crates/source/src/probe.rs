//! Reading a container's own header.
//!
//! A track loaded from a URL has no metadata until the node reads some out of
//! the bytes. Every format worth streaming states its duration and its tags in
//! the first few kilobytes, so the node reads those rather than handing the
//! stream to a separate probe process: a live stream is then identified without
//! waiting for any audio at all.

#[inline]
fn be_u24(bytes: &[u8]) -> usize {
    u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]) as usize
}

#[inline]
fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[inline]
fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

#[inline]
fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

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
        return !has_complete_box(head, b"moov");
    }
    if head.starts_with(b"OggS") {
        return size <= MAX_PROBE as i64;
    }
    if head.starts_with(b"fLaC") {
        let mut saw_streaminfo = false;
        let mut saw_comments = false;
        let mut at = 4usize;
        while let Some(header) = head.get(at..at + 4) {
            let kind = header[0] & 0x7f;
            let length = be_u24(&header[1..4]);
            let Some(next) = at.checked_add(4).and_then(|n| n.checked_add(length)) else {
                return false;
            };
            if next > head.len() {
                return !(saw_streaminfo && saw_comments);
            }
            if kind == 0 {
                saw_streaminfo = true;
            } else if kind == 4 {
                saw_comments = true;
            }
            if (saw_streaminfo && saw_comments) || (header[0] & 0x80 != 0) {
                return false;
            }
            at = next;
        }
        return !(saw_streaminfo && saw_comments);
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE") {
        let mut saw_fmt = false;
        let mut saw_data = false;
        let mut at = 12usize;
        while at + 8 <= head.len() {
            let kind = &head[at..at + 4];
            let length = le_u32(&head[at + 4..at + 8]) as usize;
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
    if valid_id3_header(head) {
        let length = synchsafe(&head[6..10]);
        let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
        let audio_start = 10usize.saturating_add(length).saturating_add(footer);
        if let Some(rest) = head.get(audio_start..) {
            if rest.starts_with(b"fLaC") {
                return can_gain_from_more(rest, size.saturating_sub(audio_start as i64));
            }
        }
        return head.len() < audio_start.saturating_add(4096);
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
        return probe_mp4(head);
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
fn for_each_box<'a>(mut data: &'a [u8], mut visit: impl FnMut(&[u8; 4], &'a [u8], bool)) {
    while let Some((header, rest)) = data.split_first_chunk::<8>() {
        let size = be_u32(&header[..4]) as usize;
        let kind: &[u8; 4] = header[4..8].try_into().unwrap();
        let (body_start, body_len) = match size {
            0 => (8, rest.len()),
            1 => {
                let Some((large, _)) = rest.split_first_chunk::<8>() else {
                    return;
                };
                let large = u64::from_be_bytes(*large);
                let Ok(large) = usize::try_from(large) else {
                    return;
                };
                let Some(body_len) = large.checked_sub(16) else {
                    return;
                };
                (16, body_len)
            }
            2..=7 => return,
            size => (8, size - 8),
        };
        let available = data.len() - body_start;
        let truncated = body_len > available;
        let take = body_len.min(available);
        visit(kind, &data[body_start..body_start + take], !truncated && size != 0);
        if truncated || body_start + take >= data.len() {
            return;
        }
        data = &data[body_start + take..];
    }
}

/// Finds a box by type among a set of boxes.
fn find_box<'a>(data: &'a [u8], want: &[u8; 4]) -> Option<&'a [u8]> {
    let mut found = None;
    for_each_box(data, |kind, body, _| {
        if kind == want && found.is_none() {
            found = Some(body);
        }
    });
    found
}

fn has_complete_box(data: &[u8], want: &[u8; 4]) -> bool {
    let mut complete = false;
    for_each_box(data, |kind, _, is_complete| {
        if kind == want && is_complete {
            complete = true;
        }
    });
    complete
}

fn probe_mp4(head: &[u8]) -> Probe {
    let mut out = Probe::default();
    // A stream that has not been indexed yet has no movie box in its head, which
    // is what a live broadcast looks like.
    let Some(moov) = find_box(head, b"moov") else {
        return out;
    };
    let (audio, audio_mdhd) = moov_audio_info(moov);
    out.audio = audio;
    let mvhd_info = find_box(moov, b"mvhd").and_then(read_mvhd);
    if let Some((timescale, duration)) = mvhd_info {
        if timescale > 0 && duration > 0 {
            out.duration_ms = milliseconds(duration as u128, timescale as u64);
        }
    }
    if out.duration_ms == 0 {
        if let Some((timescale, duration)) = audio_mdhd {
            if timescale > 0 && duration > 0 {
                out.duration_ms = milliseconds(duration as u128, timescale as u64);
            }
        }
    }
    if out.duration_ms == 0 {
        if let Some(mehd_duration) =
            find_box(moov, b"mvex").and_then(|mvex| find_box(mvex, b"mehd")).and_then(read_mehd)
        {
            let timescale = mvhd_info
                .map(|(ts, _)| ts)
                .filter(|&ts| ts > 0)
                .or_else(|| audio_mdhd.map(|(ts, _)| ts).filter(|&ts| ts > 0))
                .unwrap_or(0);
            if timescale > 0 {
                out.duration_ms = milliseconds(mehd_duration as u128, timescale as u64);
            }
        }
    }
    read_mp4_tags(moov, &mut out);
    out
}

fn moov_audio_info(moov: &[u8]) -> (bool, Option<(u32, u64)>) {
    let mut audio = false;
    let mut mdhd = None;
    for_each_box(moov, |kind, body, _| {
        if kind != b"trak" || audio {
            return;
        }
        if let Some(mdia) = find_box(body, b"mdia") {
            if let Some(handler) = find_box(mdia, b"hdlr") {
                // The handler type sits after a version, some flags and a
                // pre-defined field.
                if handler.get(8..12) == Some(b"soun") {
                    audio = true;
                    mdhd = find_box(mdia, b"mdhd").and_then(read_mvhd);
                }
            }
        }
    });
    (audio, mdhd)
}

/// Reads the timescale and duration out of a movie header.
fn read_mvhd(mvhd: &[u8]) -> Option<(u32, u64)> {
    match *mvhd.first()? {
        1 => {
            // Version one widens the creation and modification times to 64 bits,
            // which pushes the timescale to byte 20 and the duration to 24.
            let timescale = be_u32(mvhd.get(20..24)?);
            let raw = u64::from_be_bytes(mvhd.get(24..32)?.try_into().ok()?);
            let duration = if raw == u64::MAX { 0 } else { raw };
            Some((timescale, duration))
        }
        0 => {
            let timescale = be_u32(mvhd.get(12..16)?);
            let raw = be_u32(mvhd.get(16..20)?);
            let duration = if raw == u32::MAX { 0 } else { u64::from(raw) };
            Some((timescale, duration))
        }
        _ => None,
    }
}

/// Reads the fragment presentation duration out of `moov/mvex/mehd`.
fn read_mehd(mehd: &[u8]) -> Option<u64> {
    match *mehd.first()? {
        1 => {
            let duration = u64::from_be_bytes(mehd.get(4..12)?.try_into().ok()?);
            (duration > 0 && duration != u64::MAX).then_some(duration)
        }
        0 => {
            let duration = u64::from(be_u32(mehd.get(4..8)?));
            (duration > 0 && duration != u64::from(u32::MAX)).then_some(duration)
        }
        _ => None,
    }
}

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
    let mut album_artist = String::new();
    for_each_box(ilst, |kind, body, _| {
        if !matches!(kind, b"\xa9nam" | b"\xa9ART" | b"aART") {
            // Skip artwork (`covr`) and all unrelated atoms without scanning children.
            return;
        }
        let Some(data) = find_box(body, b"data") else {
            return;
        };
        let Some(value) = data.get(8..) else {
            return;
        };
        let data_type = be_u32(&data[..4]);
        let decoded = match data_type {
            1 => decode_text(value),
            2 => {
                if value.starts_with(&[0xff, 0xfe]) || value.starts_with(&[0xfe, 0xff]) {
                    decode_text(value)
                } else {
                    decode_utf16(value, false)
                }
            }
            _ => return,
        };
        if decoded.is_empty() {
            return;
        }
        match kind {
            b"\xa9nam" => out.title = decoded,
            b"\xa9ART" => out.artist = decoded,
            b"aART" if album_artist.is_empty() => album_artist = decoded,
            _ => {}
        }
    });
    if out.artist.is_empty() && !album_artist.is_empty() {
        out.artist = album_artist;
    }
}

fn clean_tag_text(raw: &str) -> String {
    let before_nul = match raw.find('\0') {
        Some(pos) => &raw[..pos],
        None => raw,
    };
    before_nul
        .trim_start_matches('\u{feff}')
        .trim()
        .trim_start_matches('\u{feff}')
        .trim()
        .to_owned()
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(
            |pair| {
                if little_endian {
                    u16::from_le_bytes(*pair)
                } else {
                    u16::from_be_bytes(*pair)
                }
            },
        )
        .take_while(|&unit| unit != 0);
    let raw: String =
        char::decode_utf16(units).map(|ch| ch.unwrap_or(char::REPLACEMENT_CHARACTER)).collect();
    clean_tag_text(&raw)
}

/// Decodes a tag value that may be UTF-8 or UTF-16.
fn decode_text(value: &[u8]) -> String {
    // A UTF-16 value starts with a byte order mark, which is what tells it apart
    // from UTF-8 that happens to hold zero bytes.
    if let Some(rest) = value.strip_prefix(&[0xff, 0xfe]) {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = value.strip_prefix(&[0xfe, 0xff]) {
        return decode_utf16(rest, false);
    }
    clean_tag_text(&String::from_utf8_lossy(value))
}

fn probe_flac(head: &[u8]) -> Probe {
    let mut out = Probe::default();
    if head.len() < 42 || (head[4] & 0x7f) != 0 || be_u24(&head[5..8]) != 34 {
        return out;
    }
    out.audio = true;
    let mut at = 4;
    while let Some(header) = head.get(at..at + 4) {
        let length = be_u24(&header[1..4]);
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
    let vendor = le_u32(&data[..4]) as usize;
    let Some(count_end) = 4usize.checked_add(vendor).and_then(|n| n.checked_add(4)) else {
        return;
    };
    let Some(count) = data.get(count_end - 4..count_end).map(|b| le_u32(b) as usize) else {
        return;
    };
    let mut at = count_end;
    for _ in 0..count.min(1024) {
        let Some(len_end) = at.checked_add(4) else {
            return;
        };
        let Some(length) = data.get(at..len_end).map(|b| le_u32(b) as usize) else {
            return;
        };
        let Some(field_end) = len_end.checked_add(length) else {
            return;
        };
        let Some(field) = data.get(len_end..field_end) else {
            return;
        };
        at = field_end;
        // Match ASCII key prefixes before UTF-8 decoding so large embedded
        // `METADATA_BLOCK_PICTURE` comments are skipped in O(1) without copying.
        if field.get(..6).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"TITLE=")) {
            out.title = String::from_utf8_lossy(&field[6..]).trim().to_owned();
        } else if field.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"ARTIST=")) {
            out.artist = String::from_utf8_lossy(&field[7..]).trim().to_owned();
        }
    }
}

fn probe_ogg(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    if head.len() < 27 || head[4] != 0 {
        return out;
    }
    let serial: [u8; 4] = head[14..18].try_into().unwrap();
    let segments = head[26] as usize;
    let Some(table) = head.get(27..27 + segments) else {
        return out;
    };
    let page1_body: usize = table.iter().map(|&n| usize::from(n)).sum();
    let page1_end = 27 + segments + page1_body;
    let Some(payload) = head.get(27 + segments..page1_end) else {
        return out;
    };
    let mut rate = 0i64;
    let mut is_flac = false;
    if payload.starts_with(b"OpusHead") && payload.len() >= 19 {
        out.audio = true;
        // Opus is always decoded at 48 kHz.
        rate = 48_000;
    } else if payload.starts_with(&[0x01, b'v', b'o', b'r', b'b', b'i', b's'])
        && payload.len() >= 16
    {
        out.audio = true;
        rate = u32::from_le_bytes(payload[12..16].try_into().unwrap()) as i64;
    } else if payload.starts_with(b"\x7fFLAC") {
        out.audio = true;
        is_flac = true;
        if let Some(streaminfo) = payload.get(17..51) {
            if payload.get(9..13) == Some(b"fLaC")
                && (payload[13] & 0x7f) == 0
                && be_u24(&payload[14..17]) == 34
            {
                let packed = u64::from_be_bytes(streaminfo[10..18].try_into().unwrap());
                let flac_rate = (packed >> 44) & 0x000f_ffff;
                let total = packed & 0x0000_000f_ffff_ffff;
                if flac_rate > 0 && total > 0 {
                    out.duration_ms = milliseconds(total as u128, flac_rate);
                }
                rate = flac_rate as i64;
            }
        }
    }
    if out.audio {
        read_ogg_tags(&head[page1_end..], &serial, is_flac, &mut out);
    }
    // The length of an Ogg stream without STREAMINFO is stated by the granule
    // of its last page, so a duration needs the tail.
    if out.duration_ms == 0 && rate > 0 && size > 0 && head.len() as i64 == size {
        if let Some(granule) = last_granule(head, &serial) {
            if granule > 0 {
                let skip = if payload.starts_with(b"OpusHead") {
                    u16::from_le_bytes([payload[10], payload[11]]) as i64
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

fn inspect_ogg_comment_packet(pkt: &[u8], is_flac: bool, out: &mut Probe) -> bool {
    if let Some(tags) = pkt.strip_prefix(b"OpusTags") {
        read_vorbis_comment(tags, out);
        return true;
    }
    if let Some(tags) = pkt.strip_prefix(&[0x03, b'v', b'o', b'r', b'b', b'i', b's']) {
        read_vorbis_comment(tags, out);
        return true;
    }
    if is_flac {
        let Some(&header) = pkt.first() else {
            return true;
        };
        if header == 0xff {
            return true;
        }
        let block_type = header & 0x7f;
        let is_last = header & 0x80 != 0;
        if block_type == 4 {
            if let Some(tags) = pkt.get(4..) {
                read_vorbis_comment(tags, out);
            }
            return true;
        }
        if block_type > 6 {
            return true;
        }
        return is_last;
    }
    true
}

fn read_ogg_tags(data: &[u8], serial: &[u8; 4], is_flac: bool, out: &mut Probe) {
    let mut at = 0usize;
    let mut assembled: Vec<u8> = Vec::new();
    let mut packets_seen = 0usize;
    while let Some(header) = data.get(at..at.saturating_add(27)) {
        if header.len() < 27 || &header[..4] != b"OggS" || header[4] != 0 {
            break;
        }
        let segments = header[26] as usize;
        let Some(table_end) = at.checked_add(27).and_then(|n| n.checked_add(segments)) else {
            break;
        };
        let Some(table) = data.get(at + 27..table_end) else {
            break;
        };
        let body_len: usize = table.iter().map(|&n| usize::from(n)).sum();
        let full_end = table_end.saturating_add(body_len);
        let available_end = full_end.min(data.len());
        let body = &data[table_end..available_end];
        let truncated_page = available_end < full_end;

        if &header[14..18] == serial {
            let continued = header[5] & 0x01 != 0;
            if !continued && !assembled.is_empty() {
                if inspect_ogg_comment_packet(&assembled, is_flac, out) {
                    return;
                }
                assembled.clear();
                packets_seen += 1;
                if packets_seen >= 16 {
                    return;
                }
            }
            let mut body_pos = 0usize;
            let mut seg_idx = 0usize;
            while seg_idx < table.len() {
                let pkt_start = body_pos;
                let mut complete_pkt = false;
                while seg_idx < table.len() {
                    let seg_len = usize::from(table[seg_idx]);
                    seg_idx += 1;
                    body_pos = body_pos.saturating_add(seg_len);
                    if seg_len < 255 {
                        complete_pkt = true;
                        break;
                    }
                }
                let slice_end = body_pos.min(body.len());
                let chunk = &body[pkt_start.min(slice_end)..slice_end];
                if slice_end < body_pos {
                    complete_pkt = false;
                }
                if assembled.is_empty() && complete_pkt {
                    if inspect_ogg_comment_packet(chunk, is_flac, out) {
                        return;
                    }
                    packets_seen += 1;
                    if packets_seen >= 16 {
                        return;
                    }
                } else {
                    let room = MAX_PROBE.saturating_sub(assembled.len());
                    assembled.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    if complete_pkt {
                        if inspect_ogg_comment_packet(&assembled, is_flac, out) {
                            return;
                        }
                        assembled.clear();
                        packets_seen += 1;
                        if packets_seen >= 16 {
                            return;
                        }
                    }
                }
                if slice_end < body_pos {
                    break;
                }
            }
        }

        if truncated_page {
            break;
        }
        at = available_end;
    }
    if !assembled.is_empty() {
        let _ = inspect_ogg_comment_packet(&assembled, is_flac, out);
    }
}

/// Reads an end-of-stream granule only when its complete page is present.
fn last_granule(data: &[u8], serial: &[u8; 4]) -> Option<i64> {
    let mut at = 0usize;
    let mut last_positive = None;
    let mut saw_eos = false;
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
        if &data[at + 14..at + 18] == serial {
            let bytes = data.get(at + 6..at + 14)?;
            let g = i64::from_le_bytes(bytes.try_into().unwrap());
            if g > 0 {
                last_positive = Some(g);
            }
            if data[at + 5] & 4 != 0 {
                saw_eos = true;
            }
        }
        at += header + body;
    }
    saw_eos.then_some(last_positive).flatten()
}

fn probe_wav(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    let mut format_tag = 0u16;
    let mut rate = 0u32;
    let mut channels = 0u32;
    let mut bits = 0u32;
    let mut avg_bytes_per_sec = 0u32;
    let mut data_len = 0i64;
    let mut data_offset = 44i64;
    let mut saw_data = false;
    let mut streaming_data = false;
    let mut at = 12usize;
    while at + 8 <= head.len() {
        let kind = &head[at..at + 4];
        let raw_len = le_u32(&head[at + 4..at + 8]);
        let length = raw_len as usize;
        let body_end = at.saturating_add(8).saturating_add(length).min(head.len());
        let body = head.get(at + 8..body_end).unwrap_or_default();
        match kind {
            b"fmt " if body.len() >= 16 => {
                format_tag = le_u16(&body[0..2]);
                channels = u32::from(le_u16(&body[2..4]));
                rate = le_u32(&body[4..8]);
                avg_bytes_per_sec = le_u32(&body[8..12]);
                bits = u32::from(le_u16(&body[14..16]));
                if channels > 0 && rate > 0 {
                    out.audio = true;
                }
            }
            b"data" => {
                saw_data = true;
                data_offset = (at + 8) as i64;
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
        if !saw_data {
            data_offset = i64::try_from(next).unwrap_or(i64::MAX).max(44);
        }
        if next > head.len() {
            break;
        }
        at = next;
    }
    let is_pcm_like = matches!(format_tag, 1 | 3 | 0xfffe);
    let bytes_per_sec = if is_pcm_like
        && rate > 0
        && channels > 0
        && bits > 0
        && (channels * bits).is_multiple_of(8)
    {
        u64::from(channels * bits / 8) * u64::from(rate)
    } else {
        u64::from(avg_bytes_per_sec)
    };
    let payload_bytes = if data_len > 0 {
        data_len
    } else if (streaming_data || !saw_data) && size > data_offset {
        size - data_offset
    } else {
        0
    };
    if out.audio && bytes_per_sec > 0 && payload_bytes > 0 {
        out.duration_ms = milliseconds(payload_bytes as u128, bytes_per_sec);
    }
    out
}

fn read_riff_info(body: &[u8], out: &mut Probe) {
    if !body.starts_with(b"INFO") {
        return;
    }
    let mut at = 4usize;
    while let Some(header) = body.get(at..at.saturating_add(8)) {
        if header.len() < 8 {
            break;
        }
        let kind = &header[..4];
        let length = le_u32(&header[4..8]) as usize;
        let Some(value_start) = at.checked_add(8) else {
            break;
        };
        let Some(value_end) = value_start.checked_add(length) else {
            break;
        };
        let Some(value) = body.get(value_start..value_end) else {
            break;
        };
        match kind {
            b"INAM" => {
                let decoded = clean_tag_text(&String::from_utf8_lossy(value));
                if !decoded.is_empty() {
                    out.title = decoded;
                }
            }
            b"IART" => {
                let decoded = clean_tag_text(&String::from_utf8_lossy(value));
                if !decoded.is_empty() {
                    out.artist = decoded;
                }
            }
            _ => {}
        }
        let Some(next) = value_end.checked_add(length & 1) else {
            break;
        };
        if next > body.len() {
            break;
        }
        at = next;
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
    if head.starts_with(b"ID3") {
        if !valid_id3_header(head) {
            return out;
        }
        saw_id3 = true;
        let flags = head[5];
        let length = synchsafe(&head[6..10]);
        // The footer, when there is one, is another ten bytes.
        let footer = if flags & 0x10 != 0 { 10 } else { 0 };
        let tag_end = 10usize.saturating_add(length).min(head.len());
        read_id3(&head[10..tag_end], head[3], flags, &mut out);
        audio_start = 10usize.saturating_add(length).saturating_add(footer);
    }
    // A stream still inside its ID3 tag has no audio bytes buffered yet.
    // Once bytes past the tag arrive, require an actual audio frame or header.
    if saw_id3 && head.len() <= audio_start {
        out.audio = true;
        return out;
    }
    let Some(rest) = head.get(audio_start..) else {
        return out;
    };
    if rest.starts_with(b"fLaC") {
        let flac = probe_flac(rest);
        out.audio = flac.audio;
        out.duration_ms = flac.duration_ms;
        if out.title.is_empty() {
            out.title = flac.title;
        }
        if out.artist.is_empty() {
            out.artist = flac.artist;
        }
        return out;
    }
    if rest.len() >= 2 && rest[0] == 0xff && (rest[1] & 0xf6) == 0xf0 {
        let adts_size = if size > audio_start as i64 { size - audio_start as i64 } else { 0 };
        let adts = probe_adts(rest, adts_size);
        out.audio = adts.audio;
        out.duration_ms = adts.duration_ms;
        return out;
    }
    let Some(frame) = find_mpeg_sync(rest) else {
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
    let mut first_single = None;
    for offset in 0..limit {
        let candidate = &data[offset..];
        let Some(mpeg) = parse_mpeg_frame(candidate) else { continue };
        if first_single.is_none() {
            first_single = Some(candidate);
        }
        if parse_vbr_frames(candidate, &mpeg).is_some() {
            return Some(candidate);
        }
        if mpeg.frame_size >= 4 {
            if let Some(next_slice) = candidate.get(mpeg.frame_size..) {
                if parse_mpeg_frame(next_slice).is_some_and(|next| next.rate == mpeg.rate) {
                    return Some(candidate);
                }
            }
        }
    }
    first_single
}

fn deunsync(data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if !data.windows(2).any(|w| w == [0xff, 0x00]) {
        return std::borrow::Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut idx = 0;
    while idx < data.len() {
        out.push(data[idx]);
        if data[idx] == 0xff && data.get(idx + 1) == Some(&0x00) {
            idx += 2;
        } else {
            idx += 1;
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Reads the frames of an ID3v2 tag that name the track.
fn read_id3(raw_data: &[u8], version: u8, flags: u8, out: &mut Probe) {
    let whole_unsync = version <= 3 && (flags & 0x80 != 0);
    let data_cow =
        if whole_unsync { deunsync(raw_data) } else { std::borrow::Cow::Borrowed(raw_data) };
    let data = data_cow.as_ref();

    if version == 2 {
        if flags & 0x40 != 0 {
            return;
        }
        let mut at = 0usize;
        while at + 6 <= data.len() {
            let id = &data[at..at + 3];
            if !id.iter().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()) {
                return;
            }
            let length = be_u24(&data[at + 3..at + 6]);
            let Some(body_end) = at.checked_add(6).and_then(|n| n.checked_add(length)) else {
                return;
            };
            let Some(body) = data.get(at + 6..body_end) else {
                return;
            };
            match id {
                b"TT2" => {
                    let decoded = decode_id3_text(body);
                    if !decoded.is_empty() {
                        out.title = decoded;
                    }
                }
                b"TP1" => {
                    let decoded = decode_id3_text(body);
                    if !decoded.is_empty() {
                        out.artist = decoded;
                    }
                }
                _ => {}
            }
            at = body_end;
        }
        return;
    }

    let mut at = 0usize;
    if flags & 0x40 != 0 {
        let Some(ext_hdr) = data.get(..4) else {
            return;
        };
        let ext_len = if version == 4 {
            synchsafe(ext_hdr)
        } else {
            4usize.saturating_add(be_u32(ext_hdr) as usize)
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
            be_u32(&data[at + 4..at + 8]) as usize
        };
        let fmt_flags = data[at + 9];
        let Some(body_end) = at.checked_add(10).and_then(|n| n.checked_add(length)) else {
            return;
        };
        let Some(mut body) = data.get(at + 10..body_end) else {
            return;
        };
        at = body_end;
        // Filter frame IDs before decoding text so multi-hundred-KiB `APIC`
        // cover art frames are skipped in O(1) without allocating a String.
        let is_title = match id {
            b"TIT2" => true,
            b"TPE1" => false,
            _ => continue,
        };
        if version == 3 {
            // Skip compressed (0x80) or encrypted (0x40) frames; strip group ID (0x20).
            if fmt_flags & 0xc0 != 0 {
                continue;
            }
            if fmt_flags & 0x20 != 0 {
                let Some(rest) = body.get(1..) else { continue };
                body = rest;
            }
        } else if version == 4 {
            // Skip compressed (0x08) or encrypted (0x04) frames.
            if fmt_flags & 0x0c != 0 {
                continue;
            }
            if fmt_flags & 0x40 != 0 {
                let Some(rest) = body.get(1..) else { continue };
                body = rest;
            }
            if fmt_flags & 0x01 != 0 {
                let Some(rest) = body.get(4..) else { continue };
                body = rest;
            }
        }
        let frame_cow = if version == 4 && ((flags & 0x80 != 0) || (fmt_flags & 0x02 != 0)) {
            deunsync(body)
        } else {
            std::borrow::Cow::Borrowed(body)
        };
        let decoded = decode_id3_text(frame_cow.as_ref());
        if decoded.is_empty() {
            continue;
        }
        if is_title {
            out.title = decoded;
        } else {
            out.artist = decoded;
        }
    }
}

fn decode_id3_text(body: &[u8]) -> String {
    let Some((&encoding, value)) = body.split_first() else {
        return String::new();
    };
    match encoding {
        // ISO-8859-1 (Latin-1).
        0 => {
            let end = value.iter().position(|&b| b == 0).unwrap_or(value.len());
            let raw: String = value[..end].iter().map(|&byte| byte as char).collect();
            clean_tag_text(&raw)
        }
        // UTF-16 with a byte order mark, or UTF-16LE when taggers omit the BOM.
        1 => {
            if let Some(rest) = value.strip_prefix(&[0xff, 0xfe]) {
                decode_utf16(rest, true)
            } else if let Some(rest) = value.strip_prefix(&[0xfe, 0xff]) {
                decode_utf16(rest, false)
            } else {
                decode_utf16(value, true)
            }
        }
        // UTF-16BE without a byte order mark.
        2 => decode_utf16(value, false),
        // UTF-8.
        3 => clean_tag_text(&String::from_utf8_lossy(value)),
        _ => String::new(),
    }
}

struct MpegFrame {
    bitrate: u32,
    rate: u32,
    samples_per_frame: u32,
    side_info_len: usize,
    frame_size: usize,
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
    let padding = ((header >> 9) & 0x1) as usize;
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
    let bitrate = kbps * 1000;
    let frame_size = if layer == 0x3 {
        ((12 * bitrate / rate) as usize + padding) * 4
    } else if version != 0x3 && layer == 0x1 {
        (72 * bitrate / rate) as usize + padding
    } else {
        (144 * bitrate / rate) as usize + padding
    };
    let mono = channel_mode == 0x3;
    let side_info_len = match (version == 0x3, mono) {
        (true, false) => 32,
        (true, true) | (false, false) => 17,
        (false, true) => 9,
    };
    Some(MpegFrame { bitrate, rate, samples_per_frame, side_info_len, frame_size })
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
    let min_hdr = if head[1] & 1 == 0 { 9 } else { 7 };
    let frame_len =
        (((head[3] as i64) & 0x3) << 11) | ((head[4] as i64) << 3) | ((head[5] as i64) >> 5);
    if rate == 0 || frame_len < min_hdr {
        return out;
    }
    out.audio = true;
    // An ADTS stream states no duration, but the frames are a fixed number of
    // samples, so the length follows from the file size and the average bitrate
    // the header implies.
    let channels = ((head[2] & 0x1) << 2) | (head[3] >> 6);
    if size > 0 && channels > 0 {
        let blocks = u128::from(head[6] & 0x03) + 1;
        let frames = size / frame_len;
        out.duration_ms = milliseconds(frames as u128 * blocks * 1024, rate as u64);
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
        out.extend_from_slice(&36u32.saturating_add(data_len).to_le_bytes());
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
        assert_eq!(last_granule(&page, &[1, 2, 3, 4]), Some(48_000));
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

    #[test]
    fn bomless_utf16_id3_invalid_encodings_and_utf8_boms_decode_cleanly() {
        let mut utf16le_no_bom = vec![1u8];
        for unit in "Song\0Extra".encode_utf16() {
            utf16le_no_bom.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_id3_text(&utf16le_no_bom), "Song");
        assert_eq!(decode_id3_text(&[4, b'b', b'a', b'd']), "");
        assert_eq!(decode_id3_text(b"\x03\xef\xbb\xbfTitle"), "Title");
    }

    #[test]
    fn id3_prefixed_flac_and_adts_keep_tags_and_reject_trailing_garbage() {
        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3\x03\x00\x00");
        tag.extend_from_slice(&[0, 0, 0, 17]);
        tag.extend_from_slice(b"TIT2\x00\x00\x00\x07\x00\x00\x00a song");

        let mut flac = tag.clone();
        flac.extend_from_slice(b"fLaC\x80\x00\x00\x22");
        flac.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0]);
        let packed: u64 = (44_100u64 << 44) | 88_200;
        flac.extend_from_slice(&packed.to_be_bytes());
        flac.extend_from_slice(&[0; 16]);
        let flac_probe = probe(&flac, flac.len() as i64);
        assert!(flac_probe.audio);
        assert_eq!(flac_probe.title, "a song");
        assert_eq!(flac_probe.duration_ms, 2000);

        let mut adts = tag.clone();
        // MPEG-4 ADTS, no CRC, 48 kHz (index 3), stereo (2), frame length = 200, 1 block.
        adts.extend_from_slice(&[0xff, 0xf1, 0x4c, 0x80, 0x19, 0x00, 0xfc]);
        let adts_probe = probe(&adts, tag.len() as i64 + 200 * 50);
        assert!(adts_probe.audio);
        assert_eq!(adts_probe.title, "a song");
        assert_eq!(adts_probe.duration_ms, 1066);

        let mut not_audio = tag;
        not_audio.extend_from_slice(b"<html>not an audio stream</html>");
        assert!(!probe(&not_audio, not_audio.len() as i64).audio);
    }

    #[test]
    fn fmp4_mehd_duration_and_album_artist_fallback_are_supported() {
        let box_of = |kind: &[u8; 4], body: &[u8]| {
            let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend_from_slice(body);
            b
        };
        let mut mvhd = vec![0u8; 20];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&0u32.to_be_bytes());
        let mut mehd = vec![0u8; 8];
        mehd[4..8].copy_from_slice(&45_000u32.to_be_bytes());
        let mut aart_data = vec![0, 0, 0, 1, 0, 0, 0, 0];
        aart_data.extend_from_slice(b"\xef\xbb\xbfAlbum Artist");
        let ilst = box_of(b"aART", &box_of(b"data", &aart_data));
        let mut meta = vec![0u8; 4];
        meta.extend(box_of(b"ilst", &ilst));
        let mut moov = box_of(b"mvhd", &mvhd);
        moov.extend(box_of(b"mvex", &box_of(b"mehd", &mehd)));
        moov.extend(box_of(b"udta", &box_of(b"meta", &meta)));
        let mut head = box_of(b"ftyp", b"isom\0\0\0\0");
        head.extend(box_of(b"moov", &moov));
        let info = probe(&head, head.len() as i64);
        assert_eq!(info.duration_ms, 45_000);
        assert_eq!(info.artist, "Album Artist");
    }

    #[test]
    fn ogg_flac_streaminfo_and_multipage_interleaved_comments_are_read() {
        let make_page = |flags: u8, serial: [u8; 4], segs: &[u8], body: &[u8]| {
            let mut p = b"OggS\0".to_vec();
            p.push(flags);
            p.extend_from_slice(&0i64.to_le_bytes());
            p.extend_from_slice(&serial);
            p.extend_from_slice(&[0u8; 8]);
            p.push(segs.len() as u8);
            p.extend_from_slice(segs);
            p.extend_from_slice(body);
            p
        };
        let mut flac_bos = b"\x7fFLAC\x01\x00\x00\x01fLaC\x00\x00\x00\x22".to_vec();
        let mut streaminfo = vec![0u8; 34];
        let packed: u64 = (48_000u64 << 44) | 144_000;
        streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());
        flac_bos.extend_from_slice(&streaminfo);
        let mut ogg_flac = make_page(2, [7, 7, 7, 7], &[flac_bos.len() as u8], &flac_bos);
        // Interleaved page of another logical stream is skipped.
        ogg_flac.extend(make_page(2, [9, 9, 9, 9], &[4], b"junk"));
        // Multi-page Vorbis comment packet for serial [7,7,7,7].
        let mut comment_pkt = vec![0x84, 0, 0, 0];
        let vendor = vec![b'v'; 260];
        comment_pkt.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        comment_pkt.extend_from_slice(&vendor);
        comment_pkt.extend_from_slice(&2u32.to_le_bytes());
        for field in ["TITLE=Ogg FLAC Song", "ARTIST=Ogg Artist"] {
            comment_pkt.extend_from_slice(&(field.len() as u32).to_le_bytes());
            comment_pkt.extend_from_slice(field.as_bytes());
        }
        let block_len = (comment_pkt.len() - 4) as u32;
        comment_pkt[1..4].copy_from_slice(&block_len.to_be_bytes()[1..]);
        ogg_flac.extend(make_page(0, [7, 7, 7, 7], &[255], &comment_pkt[..255]));
        let tail = &comment_pkt[255..];
        ogg_flac.extend(make_page(1, [7, 7, 7, 7], &[tail.len() as u8], tail));

        let info = probe(&ogg_flac, 999_999);
        assert!(info.audio);
        assert_eq!(info.duration_ms, 3000);
        assert_eq!(info.title, "Ogg FLAC Song");
        assert_eq!(info.artist, "Ogg Artist");
    }

    #[test]
    fn streaming_wav_subtracts_actual_data_offset_and_bounds_riff_info() {
        let mut head = wav(48_000, 2, 16, u32::MAX);
        let data = head.split_off(36);
        let mut info = b"INFOINAM\x04\0\0\0Keep".to_vec();
        // Append a truncated INAM chunk that must not clobber the valid title.
        info.extend_from_slice(b"INAM\xff\xff\xff\x7f");
        head.extend_from_slice(b"LIST");
        head.extend_from_slice(&(info.len() as u32).to_le_bytes());
        head.extend_from_slice(&info);
        if info.len() & 1 != 0 {
            head.push(0);
        }
        head.extend_from_slice(&data);
        let data_offset = head.len() as i64;
        let info_probe = probe(&head, data_offset + 48_000 * 4);
        assert!(info_probe.audio);
        assert_eq!(info_probe.title, "Keep");
        assert_eq!(info_probe.duration_ms, 1000);
    }
}
