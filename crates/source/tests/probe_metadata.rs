//! Container metadata is parsed from real nested headers, not isolated helpers.

use rustlink_source::probe::probe;

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut bytes = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(body);
    bytes
}

fn mp4(moov: &[u8]) -> Vec<u8> {
    let mut bytes = atom(b"ftyp", b"M4A \0\0\0\0");
    bytes.extend(atom(b"moov", moov));
    bytes
}

fn tagged_mp4(fullbox: bool) -> Vec<u8> {
    let mut tags = Vec::new();
    for (kind, text) in [(b"\xa9nam", "夜空"), (b"\xa9ART", "歌手")] {
        let mut value = vec![0, 0, 0, 1, 0, 0, 0, 0];
        value.extend_from_slice(text.as_bytes());
        tags.extend(atom(kind, &atom(b"data", &value)));
    }
    let mut meta = if fullbox { vec![0; 4] } else { Vec::new() };
    // QuickTime also permits a meta atom starting directly with this child.
    let mut handler = vec![0; 8];
    handler.extend_from_slice(b"mdir");
    handler.extend_from_slice(&[0; 12]);
    meta.extend(atom(b"hdlr", &handler));
    meta.extend(atom(b"ilst", &tags));
    mp4(&atom(b"udta", &atom(b"meta", &meta)))
}

#[test]
fn mp4_fullbox_metadata_retains_title_and_artist() {
    let bytes = tagged_mp4(true);
    let info = probe(&bytes, bytes.len() as i64);
    assert_eq!(info.title, "夜空");
    assert_eq!(info.artist, "歌手");
}

#[test]
fn quicktime_meta_without_a_fullbox_header_retains_tags() {
    let bytes = tagged_mp4(false);
    let info = probe(&bytes, bytes.len() as i64);
    assert_eq!(info.title, "夜空");
    assert_eq!(info.artist, "歌手");
}

#[test]
fn flac_comments_after_padding_are_read_with_metadata_block_lengths() {
    let mut bytes = b"fLaC\x00\x00\x00\x22".to_vec();
    let mut info = vec![0; 34];
    info[10..18].copy_from_slice(&((44_100u64 << 44) | 88_200).to_be_bytes());
    bytes.extend(info);
    bytes.extend_from_slice(b"\x01\x00\x00\x04\0\0\0\0");
    let mut comments = 0u32.to_le_bytes().to_vec();
    comments.extend_from_slice(&2u32.to_le_bytes());
    for field in ["TITLE=夜空", "ARTIST=歌手"] {
        comments.extend_from_slice(&(field.len() as u32).to_le_bytes());
        comments.extend_from_slice(field.as_bytes());
    }
    bytes.push(0x84);
    bytes.extend_from_slice(&(comments.len() as u32).to_be_bytes()[1..]);
    bytes.extend(comments);
    let result = probe(&bytes, bytes.len() as i64);
    assert_eq!(result.duration_ms, 2000);
    assert_eq!(result.title, "夜空");
    assert_eq!(result.artist, "歌手");
}

fn ogg_page(flags: u8, granule: u64, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() < 255);
    let mut page = b"OggS\0".to_vec();
    page.push(flags);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&[0; 12]);
    page.push(1);
    page.push(payload.len() as u8);
    page.extend_from_slice(payload);
    page
}

fn opus_head() -> Vec<u8> {
    let mut head = b"OpusHead\x01\x02".to_vec();
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0; 3]);
    let mut bytes = ogg_page(2, 0, &head);
    bytes.extend(ogg_page(0, 0, b"OpusTags\0\0\0\0\0\0\0\0"));
    bytes
}

#[test]
fn an_ogg_prefix_does_not_claim_the_whole_tracks_duration() {
    let mut bytes = opus_head();
    bytes.extend(ogg_page(0, 960, &[0xf8, 0xff, 0xfe]));
    let info = probe(&bytes, 1_000_000);
    assert!(info.audio);
    assert_eq!(info.duration_ms, 0, "one buffered page is not the end of a long recording");
}

#[test]
fn complete_ogg_opus_duration_excludes_preskip() {
    let mut bytes = opus_head();
    for packet in 1..=50 {
        bytes.extend(ogg_page(0, packet * 960, &[0xf8, 0xff, 0xfe]));
    }
    bytes.extend(ogg_page(4, 48_312, &[0xf8, 0xff, 0xfe]));
    assert_eq!(probe(&bytes, bytes.len() as i64).duration_ms, 1000);
}

#[test]
fn unknown_mp4_duration_does_not_become_a_huge_or_negative_track() {
    for version in [0u8, 1] {
        let mut header = vec![0; if version == 0 { 20 } else { 32 }];
        header[0] = version;
        if version == 0 {
            header[12..16].copy_from_slice(&1000u32.to_be_bytes());
            header[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        } else {
            header[20..24].copy_from_slice(&1000u32.to_be_bytes());
            header[24..32].copy_from_slice(&u64::MAX.to_be_bytes());
        }
        let bytes = mp4(&atom(b"mvhd", &header));
        assert_eq!(probe(&bytes, bytes.len() as i64).duration_ms, 0);
    }
}

#[test]
fn oversized_http_length_does_not_wrap_mp3_duration() {
    let bytes = [0xff, 0xfb, 0x90, 0x00]; // 128 kbit/s MPEG-1 Layer III.
    assert_eq!(probe(&bytes, i64::MAX).duration_ms, 576_460_752_303_423_487);
}

fn synchsafe(length: usize) -> [u8; 4] {
    [
        ((length >> 21) & 0x7f) as u8,
        ((length >> 14) & 0x7f) as u8,
        ((length >> 7) & 0x7f) as u8,
        (length & 0x7f) as u8,
    ]
}

fn id3(version: u8, frames: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut data = Vec::new();
    for (name, body) in frames {
        data.extend_from_slice(*name);
        let length =
            if version == 4 { synchsafe(body.len()) } else { (body.len() as u32).to_be_bytes() };
        data.extend_from_slice(&length);
        data.extend_from_slice(&[0; 2]);
        data.extend_from_slice(body);
    }
    let mut bytes = b"ID3".to_vec();
    bytes.extend_from_slice(&[version, 0, 0]);
    bytes.extend_from_slice(&synchsafe(data.len()));
    bytes.extend(data);
    bytes.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
    bytes
}

#[test]
fn id3v24_long_frames_keep_synchsafe_lengths_and_following_artist() {
    let title = "夜空".repeat(30);
    let mut value = vec![3];
    value.extend_from_slice(title.as_bytes());
    for version in [3, 4] {
        let bytes = id3(version, &[(b"TIT2", value.clone()), (b"TPE1", b"\x03Artist".to_vec())]);
        let info = probe(&bytes, bytes.len() as i64);
        assert_eq!(info.title, title, "ID3v2.{version} title");
        assert_eq!(info.artist, "Artist", "ID3v2.{version} artist");
    }
}

#[test]
fn id3_utf16_bom_titles_exclude_the_text_terminator() {
    for little_endian in [true, false] {
        let mut value = vec![1];
        value.extend_from_slice(if little_endian { &[0xff, 0xfe] } else { &[0xfe, 0xff] });
        for unit in "夜空\0".encode_utf16() {
            let bytes = if little_endian { unit.to_le_bytes() } else { unit.to_be_bytes() };
            value.extend_from_slice(&bytes);
        }
        let bytes = id3(3, &[(b"TIT2", value)]);
        assert_eq!(probe(&bytes, bytes.len() as i64).title, "夜空");
    }
}
