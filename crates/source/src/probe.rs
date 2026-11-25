//! Container duration and tag probing for MP4/fMP4, FLAC, Ogg, WAV, MP3/ID3v2, ADTS, and WebM.

use std::borrow::Cow;

#[inline]
fn be_u24(b: &[u8]) -> usize {
    u32::from_be_bytes([0, b[0], b[1], b[2]]) as usize
}

#[inline]
fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[inline]
fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

#[inline]
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn le_i64(b: &[u8]) -> i64 {
    i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

pub const MAX_PROBE: usize = 8 << 20;
pub const FIRST_PROBE: usize = 16 << 10;

pub fn may_index_later(head: &[u8]) -> bool {
    head.get(4..8) == Some(b"ftyp")
}

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
        let (mut saw_info, mut saw_comments, mut at) = (false, false, 4usize);
        while let Some(hdr) = head.get(at..at + 4) {
            let Some(next) = at.checked_add(4).and_then(|n| n.checked_add(be_u24(&hdr[1..4])))
            else {
                return false;
            };
            if next > head.len() {
                return !(saw_info && saw_comments);
            }
            saw_info |= (hdr[0] & 0x7f) == 0;
            saw_comments |= (hdr[0] & 0x7f) == 4;
            if (saw_info && saw_comments) || (hdr[0] & 0x80 != 0) {
                return false;
            }
            at = next;
        }
        return !(saw_info && saw_comments);
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE") {
        let (mut saw_fmt, mut saw_data, mut at) = (false, false, 12usize);
        while let Some(hdr) = head.get(at..at + 8) {
            let len = le_u32(&hdr[4..8]) as usize;
            saw_fmt |= &hdr[..4] == b"fmt " && head.len() >= at + 24;
            saw_data |= &hdr[..4] == b"data";
            if saw_fmt && saw_data {
                return false;
            }
            let Some(next) = at
                .checked_add(8)
                .and_then(|n| n.checked_add(len))
                .and_then(|n| n.checked_add(len & 1))
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
        let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
        let audio_start = 10usize.saturating_add(synchsafe(&head[6..10])).saturating_add(footer);
        if let Some(rest) = head.get(audio_start..).filter(|r| r.starts_with(b"fLaC")) {
            return can_gain_from_more(rest, size.saturating_sub(audio_start as i64));
        }
        return head.len() < audio_start.saturating_add(4096);
    }
    false
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    pub audio: bool,
    pub title: String,
    pub artist: String,
    pub artwork: Option<String>,
    pub duration_ms: i64,
}

fn milliseconds(units: u128, rate: u64) -> i64 {
    if rate == 0 {
        0
    } else {
        i64::try_from(units * 1000 / u128::from(rate)).unwrap_or(0)
    }
}

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
    if starts_mpeg_frame(head) {
        return probe_mp3(head, size);
    }
    if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Probe { audio: true, ..Probe::default() };
    }
    Probe::default()
}

fn for_each_box<'a>(mut data: &'a [u8], mut visit: impl FnMut(&[u8; 4], &'a [u8], bool)) {
    while let Some((header, rest)) = data.split_first_chunk::<8>() {
        let size = be_u32(&header[..4]) as usize;
        let kind: &[u8; 4] = header[4..8].try_into().unwrap();
        let (body_start, body_len) = match size {
            0 => (8, rest.len()),
            1 => {
                let Some((large, _)) = rest.split_first_chunk::<8>() else { return };
                let Some(len) = usize::try_from(u64::from_be_bytes(*large))
                    .ok()
                    .and_then(|n| n.checked_sub(16))
                else {
                    return;
                };
                (16, len)
            }
            2..=7 => return,
            n => (8, n - 8),
        };
        let avail = data.len() - body_start;
        let truncated = body_len > avail;
        let take = body_len.min(avail);
        visit(kind, &data[body_start..body_start + take], !truncated && size != 0);
        if truncated || body_start + take >= data.len() {
            return;
        }
        data = &data[body_start + take..];
    }
}

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
    for_each_box(data, |kind, _, ok| complete |= kind == want && ok);
    complete
}

fn probe_mp4(head: &[u8]) -> Probe {
    let mut out = Probe::default();
    let Some(moov) = find_box(head, b"moov") else { return out };
    let (audio, audio_mdhd) = moov_audio_info(moov);
    out.audio = audio;
    let mvhd = find_box(moov, b"mvhd").and_then(read_mvhd);
    let (ts, dur) = mvhd
        .filter(|&(ts, d)| ts > 0 && d > 0)
        .or_else(|| audio_mdhd.filter(|&(ts, d)| ts > 0 && d > 0))
        .or_else(|| {
            let d =
                find_box(moov, b"mvex").and_then(|x| find_box(x, b"mehd")).and_then(read_mehd)?;
            let ts = mvhd
                .map(|(t, _)| t)
                .filter(|&t| t > 0)
                .or_else(|| audio_mdhd.map(|(t, _)| t).filter(|&t| t > 0))?;
            Some((ts, d))
        })
        .unwrap_or((0, 0));
    out.duration_ms = milliseconds(u128::from(dur), u64::from(ts));
    read_mp4_tags(moov, &mut out);
    out
}

fn moov_audio_info(moov: &[u8]) -> (bool, Option<(u32, u64)>) {
    let (mut audio, mut mdhd) = (false, None);
    for_each_box(moov, |kind, body, _| {
        if let Some(mdia) = (kind == b"trak" && !audio)
            .then(|| find_box(body, b"mdia"))
            .flatten()
            .filter(|m| find_box(m, b"hdlr").and_then(|h| h.get(8..12)) == Some(b"soun"))
        {
            audio = true;
            mdhd = find_box(mdia, b"mdhd").and_then(read_mvhd);
        }
    });
    (audio, mdhd)
}

fn read_mvhd(mvhd: &[u8]) -> Option<(u32, u64)> {
    match *mvhd.first()? {
        1 => {
            let raw = be_u64(mvhd.get(24..32)?);
            Some((be_u32(mvhd.get(20..24)?), if raw == u64::MAX { 0 } else { raw }))
        }
        0 => {
            let raw = be_u32(mvhd.get(16..20)?);
            Some((be_u32(mvhd.get(12..16)?), if raw == u32::MAX { 0 } else { u64::from(raw) }))
        }
        _ => None,
    }
}

fn read_mehd(mehd: &[u8]) -> Option<u64> {
    match *mehd.first()? {
        1 => Some(be_u64(mehd.get(4..12)?)).filter(|&d| d > 0 && d != u64::MAX),
        0 => {
            Some(u64::from(be_u32(mehd.get(4..8)?))).filter(|&d| d > 0 && d != u64::from(u32::MAX))
        }
        _ => None,
    }
}

fn read_mp4_tags(moov: &[u8], out: &mut Probe) {
    let Some(ilst) = find_box(moov, b"udta")
        .and_then(|u| find_box(u, b"meta"))
        .and_then(|m| if m.get(4..8) == Some(b"hdlr") { Some(m) } else { m.get(4..) })
        .and_then(|c| find_box(c, b"ilst"))
    else {
        return;
    };
    let mut album_artist = String::new();
    for_each_box(ilst, |kind, body, _| {
        if !matches!(kind, b"\xa9nam" | b"\xa9ART" | b"aART") {
            return;
        }
        let Some(data) = find_box(body, b"data") else { return };
        let Some(value) = data.get(8..) else { return };
        let decoded = match be_u32(&data[..4]) {
            1 => decode_text(value),
            2 => decode_utf16_bom(value, false),
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
    raw.split('\0')
        .next()
        .unwrap_or(raw)
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
        .map(|p| if little_endian { u16::from_le_bytes(*p) } else { u16::from_be_bytes(*p) })
        .take_while(|&u| u != 0);
    let raw: String =
        char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect();
    clean_tag_text(&raw)
}

fn decode_utf16_bom(value: &[u8], default_le: bool) -> String {
    if let Some(rest) = value.strip_prefix(&[0xff, 0xfe]) {
        decode_utf16(rest, true)
    } else if let Some(rest) = value.strip_prefix(&[0xfe, 0xff]) {
        decode_utf16(rest, false)
    } else {
        decode_utf16(value, default_le)
    }
}

fn decode_text(value: &[u8]) -> String {
    if value.starts_with(&[0xff, 0xfe]) || value.starts_with(&[0xfe, 0xff]) {
        decode_utf16_bom(value, false)
    } else {
        clean_tag_text(&String::from_utf8_lossy(value))
    }
}

fn flac_streaminfo(block: &[u8]) -> (u64, i64) {
    let packed = be_u64(&block[10..18]);
    let rate = (packed >> 44) & 0x000f_ffff;
    let total = packed & 0x0000_000f_ffff_ffff;
    (rate, if rate > 0 && total > 0 { milliseconds(u128::from(total), rate) } else { 0 })
}

fn probe_flac(head: &[u8]) -> Probe {
    let mut out = Probe::default();
    if head.len() < 42 || (head[4] & 0x7f) != 0 || be_u24(&head[5..8]) != 34 {
        return out;
    }
    out.audio = true;
    let mut at = 4;
    while let Some(hdr) = head.get(at..at + 4) {
        let len = be_u24(&hdr[1..4]);
        let Some(block) = head.get(at + 4..at + 4 + len) else { break };
        match hdr[0] & 0x7f {
            0 if block.len() == 34 => out.duration_ms = flac_streaminfo(block).1,
            4 => read_vorbis_comment(block, &mut out),
            _ => {}
        }
        if hdr[0] & 0x80 != 0 {
            break;
        }
        at += 4 + len;
    }
    out
}

fn read_vorbis_comment(data: &[u8], out: &mut Probe) {
    if data.len() < 8 {
        return;
    }
    let Some(count_end) =
        4usize.checked_add(le_u32(&data[..4]) as usize).and_then(|n| n.checked_add(4))
    else {
        return;
    };
    let Some(count) = data.get(count_end - 4..count_end).map(|b| le_u32(b) as usize) else {
        return;
    };
    let mut at = count_end;
    for _ in 0..count.min(1024) {
        let Some(len_end) = at.checked_add(4) else { return };
        let Some(len) = data.get(at..len_end).map(|b| le_u32(b) as usize) else { return };
        let Some(field_end) = len_end.checked_add(len) else { return };
        let Some(field) = data.get(len_end..field_end) else { return };
        at = field_end;
        if field.get(..6).is_some_and(|p| p.eq_ignore_ascii_case(b"TITLE=")) {
            out.title = String::from_utf8_lossy(&field[6..]).trim().to_owned();
        } else if field.get(..7).is_some_and(|p| p.eq_ignore_ascii_case(b"ARTIST=")) {
            out.artist = String::from_utf8_lossy(&field[7..]).trim().to_owned();
        }
    }
}

struct OggPage<'a> {
    flags: u8,
    granule: i64,
    serial: [u8; 4],
    table: &'a [u8],
    body: &'a [u8],
    next_offset: usize,
    truncated: bool,
}

fn parse_ogg_page(data: &[u8], at: usize) -> Option<OggPage<'_>> {
    let hdr = data.get(at..at.checked_add(27)?)?;
    if &hdr[..4] != b"OggS" || hdr[4] != 0 {
        return None;
    }
    let segs = hdr[26] as usize;
    let table_end = at.checked_add(27)?.checked_add(segs)?;
    let table = data.get(at + 27..table_end)?;
    let body_len: usize = table.iter().map(|&n| usize::from(n)).sum();
    let full_end = table_end.saturating_add(body_len);
    let avail_end = full_end.min(data.len());
    Some(OggPage {
        flags: hdr[5],
        granule: le_i64(&hdr[6..14]),
        serial: hdr[14..18].try_into().unwrap(),
        table,
        body: &data[table_end..avail_end],
        next_offset: avail_end,
        truncated: avail_end < full_end,
    })
}

fn probe_ogg(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    let Some(first) = parse_ogg_page(head, 0).filter(|p| !p.truncated) else { return out };
    let payload = first.body;
    let (mut rate, mut is_flac) = (0i64, false);
    if payload.starts_with(b"OpusHead") && payload.len() >= 19 {
        out.audio = true;
        rate = 48_000;
    } else if payload.starts_with(&[0x01, b'v', b'o', b'r', b'b', b'i', b's'])
        && payload.len() >= 16
    {
        out.audio = true;
        rate = i64::from(le_u32(&payload[12..16]));
    } else if payload.starts_with(b"\x7fFLAC") {
        out.audio = true;
        is_flac = true;
        if let Some(info) = payload.get(17..51).filter(|_| {
            payload.get(9..13) == Some(b"fLaC")
                && (payload[13] & 0x7f) == 0
                && be_u24(&payload[14..17]) == 34
        }) {
            let (flac_rate, ms) = flac_streaminfo(info);
            out.duration_ms = ms;
            rate = flac_rate as i64;
        }
    }
    if out.audio {
        read_ogg_tags(&head[first.next_offset..], &first.serial, is_flac, &mut out);
    }
    let full_ogg = out.duration_ms == 0 && rate > 0 && size > 0 && head.len() as i64 == size;
    if let Some(granule) =
        full_ogg.then(|| last_granule(head, &first.serial)).flatten().filter(|&g| g > 0)
    {
        let skip =
            if payload.starts_with(b"OpusHead") { i64::from(le_u16(&payload[10..12])) } else { 0 };
        out.duration_ms = milliseconds(granule.saturating_sub(skip).max(0) as u128, rate as u64);
    }
    out
}

fn inspect_ogg_comment_packet(pkt: &[u8], is_flac: bool, out: &mut Probe) -> bool {
    if let Some(tags) = pkt
        .strip_prefix(b"OpusTags")
        .or_else(|| pkt.strip_prefix(&[0x03, b'v', b'o', b'r', b'b', b'i', b's']))
    {
        read_vorbis_comment(tags, out);
        return true;
    }
    if !is_flac {
        return true;
    }
    let Some(&hdr) = pkt.first() else { return true };
    if hdr == 0xff {
        return true;
    }
    let kind = hdr & 0x7f;
    if kind == 4 {
        if let Some(tags) = pkt.get(4..) {
            read_vorbis_comment(tags, out);
        }
        return true;
    }
    kind > 6 || (hdr & 0x80 != 0)
}

fn read_ogg_tags(data: &[u8], serial: &[u8; 4], is_flac: bool, out: &mut Probe) {
    let (mut at, mut packets) = (0usize, 0usize);
    let mut assembled = Vec::new();
    while let Some(page) = parse_ogg_page(data, at) {
        if &page.serial == serial {
            if page.flags & 0x01 == 0 && !assembled.is_empty() {
                if inspect_ogg_comment_packet(&assembled, is_flac, out) {
                    return;
                }
                assembled.clear();
                packets += 1;
                if packets >= 16 {
                    return;
                }
            }
            let (mut body_pos, mut seg_idx) = (0usize, 0usize);
            while seg_idx < page.table.len() {
                let pkt_start = body_pos;
                let mut complete = false;
                while seg_idx < page.table.len() {
                    let seg_len = usize::from(page.table[seg_idx]);
                    seg_idx += 1;
                    body_pos = body_pos.saturating_add(seg_len);
                    if seg_len < 255 {
                        complete = true;
                        break;
                    }
                }
                let slice_end = body_pos.min(page.body.len());
                let chunk = &page.body[pkt_start.min(slice_end)..slice_end];
                complete &= slice_end == body_pos;
                if assembled.is_empty() && complete {
                    if inspect_ogg_comment_packet(chunk, is_flac, out) {
                        return;
                    }
                    packets += 1;
                    if packets >= 16 {
                        return;
                    }
                } else {
                    let room = MAX_PROBE.saturating_sub(assembled.len());
                    assembled.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    if complete {
                        if inspect_ogg_comment_packet(&assembled, is_flac, out) {
                            return;
                        }
                        assembled.clear();
                        packets += 1;
                        if packets >= 16 {
                            return;
                        }
                    }
                }
                if slice_end < body_pos {
                    break;
                }
            }
        }
        if page.truncated {
            break;
        }
        at = page.next_offset;
    }
    if !assembled.is_empty() {
        let _ = inspect_ogg_comment_packet(&assembled, is_flac, out);
    }
}

fn last_granule(data: &[u8], serial: &[u8; 4]) -> Option<i64> {
    let (mut at, mut last_pos, mut saw_eos) = (0usize, None, false);
    while let Some(page) = parse_ogg_page(data, at) {
        if page.truncated {
            break;
        }
        if &page.serial == serial {
            if page.granule > 0 {
                last_pos = Some(page.granule);
            }
            saw_eos |= page.flags & 4 != 0;
        }
        at = page.next_offset;
    }
    saw_eos.then_some(last_pos).flatten()
}

fn probe_wav(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    let (mut format_tag, mut rate, mut channels, mut bits, mut avg_bps) =
        (0u16, 0u32, 0u32, 0u32, 0u32);
    let (mut data_len, mut data_offset, mut saw_data, mut streaming) = (0i64, 44i64, false, false);
    let mut at = 12usize;
    while let Some(hdr) = head.get(at..at + 8) {
        let raw_len = le_u32(&hdr[4..8]);
        let len = raw_len as usize;
        let body = head
            .get(at + 8..at.saturating_add(8).saturating_add(len).min(head.len()))
            .unwrap_or_default();
        match &hdr[..4] {
            b"fmt " if body.len() >= 16 => {
                format_tag = le_u16(&body[0..2]);
                channels = u32::from(le_u16(&body[2..4]));
                rate = le_u32(&body[4..8]);
                avg_bps = le_u32(&body[8..12]);
                bits = u32::from(le_u16(&body[14..16]));
                out.audio |= channels > 0 && rate > 0;
            }
            b"data" => {
                saw_data = true;
                data_offset = (at + 8) as i64;
                if raw_len == u32::MAX {
                    streaming = true;
                } else {
                    data_len = len as i64;
                }
            }
            b"LIST" => read_riff_info(body, &mut out),
            _ => {}
        }
        let Some(next) =
            at.checked_add(8).and_then(|n| n.checked_add(len)).and_then(|n| n.checked_add(len & 1))
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
    let bps = if matches!(format_tag, 1 | 3 | 0xfffe)
        && rate > 0
        && channels > 0
        && bits > 0
        && (channels * bits).is_multiple_of(8)
    {
        u64::from(channels * bits / 8) * u64::from(rate)
    } else {
        u64::from(avg_bps)
    };
    let payload_bytes = if data_len > 0 {
        data_len
    } else if (streaming || !saw_data) && size > data_offset {
        size - data_offset
    } else {
        0
    };
    if out.audio && bps > 0 && payload_bytes > 0 {
        out.duration_ms = milliseconds(payload_bytes as u128, bps);
    }
    out
}

fn read_riff_info(body: &[u8], out: &mut Probe) {
    if !body.starts_with(b"INFO") {
        return;
    }
    let mut at = 4usize;
    while let Some(hdr) = body.get(at..at.saturating_add(8)) {
        if hdr.len() < 8 {
            break;
        }
        let len = le_u32(&hdr[4..8]) as usize;
        let Some(val_end) = at.checked_add(8).and_then(|n| n.checked_add(len)) else { break };
        let Some(val) = body.get(at + 8..val_end) else { break };
        if matches!(&hdr[..4], b"INAM" | b"IART") {
            let decoded = clean_tag_text(&String::from_utf8_lossy(val));
            if !decoded.is_empty() {
                if &hdr[..4] == b"INAM" {
                    out.title = decoded;
                } else {
                    out.artist = decoded;
                }
            }
        }
        let Some(next) = val_end.checked_add(len & 1).filter(|&n| n <= body.len()) else { break };
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
        && head[6..10].iter().all(|b| b & 0x80 == 0)
}

fn probe_mp3(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    let (mut audio_start, mut saw_id3) = (0usize, false);
    if head.starts_with(b"ID3") {
        if !valid_id3_header(head) {
            return out;
        }
        saw_id3 = true;
        let flags = head[5];
        let len = synchsafe(&head[6..10]);
        let footer = if flags & 0x10 != 0 { 10 } else { 0 };
        read_id3(&head[10..10usize.saturating_add(len).min(head.len())], head[3], flags, &mut out);
        audio_start = 10usize.saturating_add(len).saturating_add(footer);
    }
    if saw_id3 && head.len() <= audio_start {
        out.audio = true;
        return out;
    }
    let Some(rest) = head.get(audio_start..) else { return out };
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
        let adts = probe_adts(rest, (size - audio_start as i64).max(0));
        out.audio = adts.audio;
        out.duration_ms = adts.duration_ms;
        return out;
    }
    let Some(frame) = find_mpeg_sync(rest) else { return out };
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
    let mut first_single = None;
    for offset in 0..data.len().min(4096) {
        let candidate = &data[offset..];
        let Some(mpeg) = parse_mpeg_frame(candidate) else { continue };
        if first_single.is_none() {
            first_single = Some(candidate);
        }
        if parse_vbr_frames(candidate, &mpeg).is_some()
            || (mpeg.frame_size >= 4
                && candidate
                    .get(mpeg.frame_size..)
                    .and_then(parse_mpeg_frame)
                    .is_some_and(|next| next.rate == mpeg.rate))
        {
            return Some(candidate);
        }
    }
    first_single
}

fn deunsync(data: &[u8]) -> Cow<'_, [u8]> {
    if !data.windows(2).any(|w| w == [0xff, 0x00]) {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut idx = 0;
    while idx < data.len() {
        out.push(data[idx]);
        idx += if data[idx] == 0xff && data.get(idx + 1) == Some(&0x00) { 2 } else { 1 };
    }
    Cow::Owned(out)
}

fn read_id3(raw_data: &[u8], version: u8, flags: u8, out: &mut Probe) {
    let data_cow = if version <= 3 && (flags & 0x80 != 0) {
        deunsync(raw_data)
    } else {
        Cow::Borrowed(raw_data)
    };
    let data = data_cow.as_ref();

    if version == 2 {
        if flags & 0x40 != 0 {
            return;
        }
        let mut at = 0usize;
        while let Some(hdr) = data.get(at..at + 6) {
            if !hdr[..3].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
                return;
            }
            let Some(end) = at.checked_add(6).and_then(|n| n.checked_add(be_u24(&hdr[3..6])))
            else {
                return;
            };
            let Some(body) = data.get(at + 6..end) else { return };
            at = end;
            if matches!(&hdr[..3], b"TT2" | b"TP1") {
                let decoded = decode_id3_text(body);
                if !decoded.is_empty() {
                    if &hdr[..3] == b"TT2" {
                        out.title = decoded;
                    } else {
                        out.artist = decoded;
                    }
                }
            }
        }
        return;
    }

    let mut at = 0usize;
    if flags & 0x40 != 0 {
        let Some(ext) = data.get(..4) else { return };
        let ext_len =
            if version == 4 { synchsafe(ext) } else { 4usize.saturating_add(be_u32(ext) as usize) };
        if ext_len < 4 || ext_len > data.len() {
            return;
        }
        at = ext_len;
    }
    while let Some(hdr) = data.get(at..at + 10) {
        if !hdr[..4].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
            return;
        }
        let len = if version == 4 { synchsafe(&hdr[4..8]) } else { be_u32(&hdr[4..8]) as usize };
        let fmt_flags = hdr[9];
        let Some(end) = at.checked_add(10).and_then(|n| n.checked_add(len)) else { return };
        let Some(mut body) = data.get(at + 10..end) else { return };
        at = end;
        let is_title = match &hdr[..4] {
            b"TIT2" => true,
            b"TPE1" => false,
            _ => continue,
        };
        if version == 3 {
            if fmt_flags & 0xc0 != 0 {
                continue;
            }
            if fmt_flags & 0x20 != 0 {
                let Some(rest) = body.get(1..) else { continue };
                body = rest;
            }
        } else if version == 4 {
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
            Cow::Borrowed(body)
        };
        let decoded = decode_id3_text(&frame_cow);
        if !decoded.is_empty() {
            if is_title {
                out.title = decoded;
            } else {
                out.artist = decoded;
            }
        }
    }
}

fn decode_id3_text(body: &[u8]) -> String {
    let Some((&enc, val)) = body.split_first() else { return String::new() };
    match enc {
        0 => {
            let end = val.iter().position(|&b| b == 0).unwrap_or(val.len());
            clean_tag_text(&val[..end].iter().map(|&b| b as char).collect::<String>())
        }
        1 => decode_utf16_bom(val, true),
        2 => decode_utf16(val, false),
        3 => clean_tag_text(&String::from_utf8_lossy(val)),
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

fn parse_mpeg_frame(data: &[u8]) -> Option<MpegFrame> {
    if data.len() < 4 || !starts_mpeg_frame(data) {
        return None;
    }
    let header = be_u32(&data[..4]);
    let version = (header >> 19) & 0x3;
    let layer = (header >> 17) & 0x3;
    let bitrate_idx = ((header >> 12) & 0xf) as usize;
    let rate_idx = ((header >> 10) & 0x3) as usize;
    let padding = ((header >> 9) & 0x1) as usize;
    if version == 0x1 || layer == 0 || rate_idx >= 3 || matches!(bitrate_idx, 0 | 15) {
        return None;
    }
    let base_rate = [44_100u32, 48_000, 32_000][rate_idx];
    let rate = match version {
        0x3 => base_rate,
        0x2 => base_rate / 2,
        0x0 => base_rate / 4,
        _ => return None,
    };
    let kbps = match (version, layer) {
        (0x3, 0x1) => {
            [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0][bitrate_idx]
        }
        (0x3, 0x2) => {
            [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0][bitrate_idx]
        }
        (0x3, 0x3) => {
            [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0][bitrate_idx]
        }
        (_, 0x3) => {
            [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0][bitrate_idx]
        }
        _ => [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0][bitrate_idx],
    };
    let samples_per_frame = match (version, layer) {
        (_, 0x3) => 384,
        (0x3, _) | (_, 0x2) => 1152,
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
    let mono = ((header >> 6) & 0x3) == 0x3;
    let side_info_len = match (version == 0x3, mono) {
        (true, false) => 32,
        (true, true) | (false, false) => 17,
        (false, true) => 9,
    };
    Some(MpegFrame { bitrate, rate, samples_per_frame, side_info_len, frame_size })
}

fn parse_vbr_frames(frame: &[u8], mpeg: &MpegFrame) -> Option<u32> {
    let xing_at = 4 + mpeg.side_info_len;
    frame
        .get(xing_at..xing_at + 12)
        .filter(|t| matches!(&t[..4], b"Xing" | b"Info") && (t[7] & 0x01 != 0))
        .map(|t| be_u32(&t[8..12]))
        .filter(|&n| n > 0)
        .or_else(|| {
            frame
                .get(36..54)
                .filter(|v| &v[..4] == b"VBRI")
                .map(|v| be_u32(&v[14..18]))
                .filter(|&n| n > 0)
        })
}

fn synchsafe(data: &[u8]) -> usize {
    data.iter().fold(0usize, |v, &b| (v << 7) | usize::from(b & 0x7f))
}

const ADTS_RATES: [u32; 16] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350, 0, 0, 0,
];

fn probe_adts(head: &[u8], size: i64) -> Probe {
    let mut out = Probe::default();
    if head.len() < 7 {
        return out;
    }
    let rate = ADTS_RATES[usize::from((head[2] >> 2) & 0xf)];
    let min_hdr = if head[1] & 1 == 0 { 9 } else { 7 };
    let frame_len =
        ((i64::from(head[3]) & 0x3) << 11) | (i64::from(head[4]) << 3) | (i64::from(head[5]) >> 5);
    if rate == 0 || frame_len < min_hdr {
        return out;
    }
    out.audio = true;
    let channels = ((head[2] & 0x1) << 2) | (head[3] >> 6);
    if size > 0 && channels > 0 {
        let blocks = u128::from(head[6] & 0x03) + 1;
        out.duration_ms = milliseconds((size / frame_len) as u128 * blocks * 1024, u64::from(rate));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, bits: u16, data_len: u32) -> Vec<u8> {
        let block_align = channels * bits / 8;
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&36u32.saturating_add(data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out
    }

    fn box_of(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn a_wav_states_its_length_and_its_rate() {
        let head = wav(48_000, 2, 16, 48_000 * 4);
        let p = probe(&head, head.len() as i64);
        assert!(p.audio && p.duration_ms == 1000);
    }

    #[test]
    fn a_wav_keeps_the_names_it_was_given() {
        let mut head = wav(44_100, 1, 16, 44_100 * 2);
        let data = head.split_off(36);
        let info = b"INFOINAM\x04\0\0\0nameIART\x06\0\0\0artist";
        head.extend_from_slice(b"LIST");
        head.extend_from_slice(&(info.len() as u32).to_le_bytes());
        head.extend_from_slice(info);
        head.extend_from_slice(&data);
        let p = probe(&head, head.len() as i64);
        assert_eq!((p.title.as_str(), p.artist.as_str()), ("name", "artist"));
    }

    #[test]
    fn an_id3_tag_is_read_before_the_frames() {
        let mut tag =
            b"ID3\x03\x00\x00\x00\x00\x00\x11TIT2\x00\x00\x00\x07\x00\x00\x00a song".to_vec();
        assert_eq!(probe(&tag, tag.len() as i64).title, "a song");
        tag.clear();
    }

    #[test]
    fn an_mp3_duration_follows_from_its_bitrate() {
        let mut head = vec![0xff, 0xfb, 0x90, 0x00];
        head.resize(64 * 1024, 0);
        let p = probe(&head, head.len() as i64);
        assert!(p.audio && (3900..4100).contains(&p.duration_ms));
    }

    #[test]
    fn a_flac_states_its_length_in_its_stream_info() {
        let mut head = b"fLaC\x00\x00\x00\x22\x10\x00\x10\x00\0\0\0\0\0\0".to_vec();
        head.extend_from_slice(&((44_100u64 << 44) | 88_200).to_be_bytes());
        head.extend_from_slice(&[0; 16]);
        let p = probe(&head, head.len() as i64);
        assert!(p.audio && p.duration_ms == 2000);
    }

    #[test]
    fn a_vorbis_comment_names_the_track() {
        let mut block = 8u32.to_le_bytes().to_vec();
        block.extend_from_slice(b"rustlink");
        block.extend_from_slice(&2u32.to_le_bytes());
        for field in ["TITLE=a song", "ARTIST=someone"] {
            block.extend_from_slice(&(field.len() as u32).to_le_bytes());
            block.extend_from_slice(field.as_bytes());
        }
        let mut out = Probe::default();
        read_vorbis_comment(&block, &mut out);
        assert_eq!((out.title.as_str(), out.artist.as_str()), ("a song", "someone"));
    }

    #[test]
    fn an_ogg_page_states_the_granule_it_ends_at() {
        let mut page = b"OggS\x00\x04".to_vec();
        page.extend_from_slice(&48_000i64.to_le_bytes());
        page.extend_from_slice(&[1, 2, 3, 4]);
        page.extend_from_slice(&[0u8; 8]);
        page.extend_from_slice(&[1, 255]);
        page.resize(27 + 1 + 255, 0);
        assert_eq!(last_granule(&page, &[1, 2, 3, 4]), Some(48_000));
    }

    #[test]
    fn an_ogg_opus_stream_is_recognised() {
        let mut head = b"OggS".to_vec();
        head.extend_from_slice(&[0u8; 22]);
        head.extend_from_slice(&[1, 19]);
        head.extend_from_slice(b"OpusHead");
        head.extend_from_slice(&[1, 2, 0, 0, 0x80, 0xbb, 0, 0, 0, 0, 0]);
        assert!(probe(&head, 0).audio);
    }

    #[test]
    fn bytes_that_are_not_media_are_not_audio() {
        assert!(!probe(b"<html><body>not audio</body></html>", 100).audio);
    }

    #[test]
    fn an_mp4_states_its_length_in_its_movie_header() {
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&30_000u32.to_be_bytes());
        let mut head = box_of(b"ftyp", b"M4A \0\0\0\0");
        head.extend(box_of(b"wide", b""));
        head.extend(box_of(b"moov", &box_of(b"mvhd", &mvhd)));
        assert_eq!(probe(&head, head.len() as i64).duration_ms, 30_000);
    }

    #[test]
    fn vbr_mp3_uses_xing_frame_count_instead_of_header_frame_bitrate() {
        let mut head = vec![0xff, 0xfb, 0x10, 0x00];
        head.resize(36, 0);
        head.extend_from_slice(b"Xing\x00\x00\x00\x01");
        head.extend_from_slice(&3828u32.to_be_bytes());
        head.resize(1024, 0);
        let p = probe(&head, 3_200_000);
        assert!(p.audio && (99_900..=100_100).contains(&p.duration_ms));
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
        let tag = b"ID3\x03\x00\x00\x00\x00\x00\x11TIT2\x00\x00\x00\x07\x00\x00\x00a song".to_vec();

        let mut flac = tag.clone();
        flac.extend_from_slice(b"fLaC\x80\x00\x00\x22\x10\x00\x10\x00\0\0\0\0\0\0");
        flac.extend_from_slice(&((44_100u64 << 44) | 88_200).to_be_bytes());
        flac.extend_from_slice(&[0; 16]);
        let fp = probe(&flac, flac.len() as i64);
        assert!(fp.audio && fp.title == "a song" && fp.duration_ms == 2000);

        let mut adts = tag.clone();
        adts.extend_from_slice(&[0xff, 0xf1, 0x4c, 0x80, 0x19, 0x00, 0xfc]);
        let ap = probe(&adts, tag.len() as i64 + 200 * 50);
        assert!(ap.audio && ap.title == "a song" && ap.duration_ms == 1066);

        let mut not_audio = tag;
        not_audio.extend_from_slice(b"<html>not an audio stream</html>");
        assert!(!probe(&not_audio, not_audio.len() as i64).audio);
    }

    #[test]
    fn fmp4_mehd_duration_and_album_artist_fallback_are_supported() {
        let mut mvhd = vec![0u8; 20];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
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
        assert_eq!((info.duration_ms, info.artist.as_str()), (45_000, "Album Artist"));
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
        streaminfo[10..18].copy_from_slice(&((48_000u64 << 44) | 144_000).to_be_bytes());
        flac_bos.extend_from_slice(&streaminfo);
        let mut ogg_flac = make_page(2, [7, 7, 7, 7], &[flac_bos.len() as u8], &flac_bos);
        ogg_flac.extend(make_page(2, [9, 9, 9, 9], &[4], b"junk"));
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
        assert!(info.audio && info.duration_ms == 3000);
        assert_eq!((info.title.as_str(), info.artist.as_str()), ("Ogg FLAC Song", "Ogg Artist"));
    }

    #[test]
    fn streaming_wav_subtracts_actual_data_offset_and_bounds_riff_info() {
        let mut head = wav(48_000, 2, 16, u32::MAX);
        let data = head.split_off(36);
        let mut info = b"INFOINAM\x04\0\0\0Keep".to_vec();
        info.extend_from_slice(b"INAM\xff\xff\xff\x7f");
        head.extend_from_slice(b"LIST");
        head.extend_from_slice(&(info.len() as u32).to_le_bytes());
        head.extend_from_slice(&info);
        if info.len() & 1 != 0 {
            head.push(0);
        }
        head.extend_from_slice(&data);
        let p = probe(&head, head.len() as i64 + 48_000 * 4);
        assert!(p.audio && p.title == "Keep" && p.duration_ms == 1000);
    }
}
