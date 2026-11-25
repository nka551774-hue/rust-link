//! Streaming Ogg Opus packet reader with reusable caller packet buffers.

use std::io::{self, Read};

const PAGE_HEADER: usize = 27;
const MAX_PACKET: usize = 1 << 20;
/// `OpusTags` may carry embedded cover art larger than an audio packet.
const MAX_TAGS_PACKET: usize = 64 << 20;
const READ_CHUNK: usize = 16 * 1024;

#[inline]
fn bad_data(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[inline]
fn unexpected_eof(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, msg)
}

struct PageState {
    table: usize,
    count: usize,
    next_segment: usize,
    offset: usize,
    granule: u64,
    continued: bool,
    end_of_stream: bool,
    last_complete: usize,
}

pub struct OggOpusReader<R: Read> {
    inner: R,
    buffer: Vec<u8>,
    consumed: usize,
    filled: usize,
    serial: Option<u32>,
    page: Option<PageState>,
    packets: u64,
    pre_skip: u32,
    drop_samples: u32,
    output_gain: i16,
    ended: bool,
    packet_page_end: Option<(u64, bool)>,
}

impl<R: Read> OggOpusReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            consumed: 0,
            filled: 0,
            serial: None,
            page: None,
            packets: 0,
            pre_skip: 0,
            drop_samples: 0,
            output_gain: 0,
            ended: false,
            packet_page_end: None,
        }
    }

    pub fn pre_skip(&self) -> u32 {
        self.pre_skip
    }

    /// Returns the next audio packet, or `None` at EOF.
    pub fn next_packet(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut packet = Vec::new();
        self.next_packet_into(&mut packet).map(|found| found.then_some(packet))
    }

    /// Clears `packet` and reads the next audio packet into it (`false` at EOF).
    pub fn next_packet_into(&mut self, packet: &mut Vec<u8>) -> io::Result<bool> {
        packet.clear();
        self.append_next_packet(packet)
    }

    /// Appends the next audio packet onto `packet` without clearing prior bytes.
    #[inline]
    pub(crate) fn append_next_packet(&mut self, packet: &mut Vec<u8>) -> io::Result<bool> {
        if self.packets >= 2 {
            self.append_raw_packet(packet)
        } else {
            self.append_first_audio_packet(packet)
        }
    }

    #[cold]
    fn append_first_audio_packet(&mut self, packet: &mut Vec<u8>) -> io::Result<bool> {
        let base = packet.len();
        if self.packets == 0 {
            if !self.append_raw_packet(packet)? {
                return Ok(false);
            }
            let (pre_skip, output_gain) = read_head(&packet[base..])?;
            packet.truncate(base);
            self.pre_skip = pre_skip;
            self.drop_samples = pre_skip;
            self.output_gain = output_gain;
            self.packets = 1;
        }
        if self.packets == 1 {
            self.skip_opus_tags()?;
            self.packets = 2;
        }
        self.append_raw_packet(packet)
    }

    fn skip_opus_tags(&mut self) -> io::Result<()> {
        let mut magic = [0u8; 8];
        let mut magic_len = 0usize;
        let found = self.read_laced_packet(
            MAX_TAGS_PACKET,
            "OpusTags packet too large",
            false,
            true,
            |span| {
                let take = (8 - magic_len).min(span.len());
                magic[magic_len..magic_len + take].copy_from_slice(&span[..take]);
                magic_len += take;
            },
        )?;
        if !found || &magic[..magic_len] != b"OpusTags" {
            return Err(bad_data("missing OpusTags"));
        }
        Ok(())
    }

    fn append_raw_packet(&mut self, packet: &mut Vec<u8>) -> io::Result<bool> {
        self.read_laced_packet(MAX_PACKET, "ogg packet too large", true, false, |span| {
            packet.extend_from_slice(span);
        })
    }

    #[inline(always)]
    fn read_laced_packet(
        &mut self,
        limit: usize,
        too_large_msg: &'static str,
        record_granule: bool,
        require_packet: bool,
        mut on_span: impl FnMut(&[u8]),
    ) -> io::Result<bool> {
        let mut total = 0usize;
        let mut started = false;
        self.packet_page_end = None;
        loop {
            if self.page.is_none() {
                if !self.have_page()? {
                    if !started && !require_packet {
                        return Ok(false);
                    }
                    return Err(unexpected_eof(if require_packet {
                        "missing OpusTags"
                    } else {
                        "truncated ogg packet"
                    }));
                }
                self.page = Some(self.read_page_header()?);
            }
            let page = self.page.as_mut().expect("a page was just read");
            if page.next_segment == 0 && page.continued != started {
                self.page = None;
                return Err(bad_data("ogg page continuation mismatch"));
            }
            let segs = &self.buffer[page.table + page.next_segment..page.table + page.count];
            let (consumed_segs, chunk_len, done) = match segs.iter().position(|&s| s < 255) {
                Some(i) => (i + 1, i * 255 + usize::from(segs[i]), true),
                None => (segs.len(), segs.len() * 255, false),
            };
            let start = page.offset;
            let end = start + chunk_len;
            if end > self.filled {
                return Err(unexpected_eof("truncated ogg page"));
            }
            total = total.saturating_add(chunk_len);
            if total > limit {
                return Err(bad_data(too_large_msg));
            }
            on_span(&self.buffer[start..end]);
            page.next_segment += consumed_segs;
            page.offset = end;
            self.consumed = end;
            if done {
                if page.next_segment == page.last_complete {
                    if record_granule {
                        self.packet_page_end = Some((page.granule, page.end_of_stream));
                    }
                    self.ended |= page.end_of_stream;
                }
                if page.next_segment >= page.count {
                    self.page = None;
                }
                return Ok(true);
            }
            self.ended |= page.end_of_stream;
            started |= consumed_segs > 0;
            self.page = None;
        }
    }

    fn have_page(&mut self) -> io::Result<bool> {
        if self.ended {
            return Ok(false);
        }
        loop {
            self.compact();
            let unconsumed = &self.buffer[self.consumed..self.filled];
            let available = unconsumed.len();
            if available >= 4 && &unconsumed[..4] != b"OggS" {
                return Err(bad_data("not an ogg page"));
            }
            if available >= 5 && unconsumed[4] != 0 {
                return Err(bad_data("unsupported ogg page version"));
            }
            if available >= PAGE_HEADER {
                let total = PAGE_HEADER + usize::from(unconsumed[26]);
                if let Some(table) = unconsumed.get(PAGE_HEADER..total) {
                    let payload: usize = table.iter().map(|&n| usize::from(n)).sum();
                    if available >= total + payload {
                        return Ok(true);
                    }
                }
            }
            if !self.fill()? {
                self.ended = true;
                return if available == 0 {
                    Ok(false)
                } else {
                    Err(unexpected_eof("truncated ogg page"))
                };
            }
        }
    }

    fn read_page_header(&mut self) -> io::Result<PageState> {
        let unconsumed = self
            .buffer
            .get(self.consumed..self.filled)
            .filter(|b| b.len() >= PAGE_HEADER)
            .ok_or_else(|| unexpected_eof("truncated ogg page"))?;
        let serial = u32::from_le_bytes(unconsumed[14..18].try_into().unwrap());
        if *self.serial.get_or_insert(serial) != serial {
            return Err(bad_data("multiplexed ogg stream requires ffmpeg"));
        }
        let count = usize::from(unconsumed[26]);
        let table = self.consumed + PAGE_HEADER;
        let granule = u64::from_le_bytes(unconsumed[6..14].try_into().unwrap());
        let continued = (unconsumed[5] & 0x01) != 0;
        let end_of_stream = (unconsumed[5] & 0x04) != 0;
        let segments = &unconsumed[PAGE_HEADER..PAGE_HEADER + count];
        let last_complete = segments.iter().rposition(|&size| size < 255).map_or(0, |i| i + 1);
        if end_of_stream && segments.last() == Some(&255) {
            return Err(unexpected_eof("unfinished final ogg packet"));
        }
        Ok(PageState {
            table,
            count,
            next_segment: 0,
            offset: table + count,
            granule,
            continued,
            end_of_stream,
            last_complete,
        })
    }

    fn compact(&mut self) {
        if self.consumed > 0 && (self.consumed == self.filled || self.consumed >= READ_CHUNK) {
            self.buffer.copy_within(self.consumed..self.filled, 0);
            self.filled -= self.consumed;
            self.consumed = 0;
        }
    }

    fn fill(&mut self) -> io::Result<bool> {
        if self.buffer.len() - self.filled < READ_CHUNK {
            self.buffer.resize(self.filled + READ_CHUNK, 0);
        }
        loop {
            match self.inner.read(&mut self.buffer[self.filled..self.filled + READ_CHUNK]) {
                Ok(read) => {
                    self.filled += read;
                    return Ok(read > 0);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub fn take_drop_samples(&mut self) -> u32 {
        std::mem::take(&mut self.drop_samples)
    }

    pub fn output_gain(&self) -> i16 {
        self.output_gain
    }

    /// EOS granule (48 kHz samples including pre-skip) for the most recently completed page.
    pub fn packet_end_granule(&self) -> Option<u64> {
        self.packet_page_end.and_then(|(granule, eos)| eos.then_some(granule))
    }

    pub(crate) fn packet_page_end(&self) -> Option<(u64, bool)> {
        self.packet_page_end
    }
}

fn read_head(packet: &[u8]) -> io::Result<(u32, i16)> {
    if packet.len() < 19 || !packet.starts_with(b"OpusHead") || packet[8] > 15 {
        return Err(bad_data("invalid OpusHead"));
    }
    if !(1..=2).contains(&packet[9]) || packet[18] != 0 {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported Opus channel mapping"));
    }
    Ok((
        u32::from(u16::from_le_bytes([packet[10], packet[11]])),
        i16::from_le_bytes([packet[16], packet[17]]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(header_type: u8, sequence: u32, segments: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(PAGE_HEADER + segments.len() + payload.len());
        out.extend_from_slice(&[b'O', b'g', b'g', b'S', 0, header_type]);
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&sequence.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(segments.len() as u8);
        out.extend_from_slice(segments);
        out.extend_from_slice(payload);
        out
    }

    fn opus_head(pre_skip: u16) -> Vec<u8> {
        let mut head = b"OpusHead\x01\x02".to_vec();
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        head
    }

    fn stream_with_audio(pages: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
        let head = opus_head(312);
        let tags = b"OpusTags\x00\x00\x00\x00";
        let mut stream = page(2, 0, &[head.len() as u8], &head);
        stream.extend_from_slice(&page(0, 1, &[tags.len() as u8], tags));
        for entry in pages {
            stream.extend_from_slice(&entry);
        }
        stream
    }

    #[test]
    fn the_headers_are_skipped_and_the_audio_packets_come_back_in_order() {
        let stream = stream_with_audio([page(0, 2, &[3, 4], &[1, 2, 3, 4, 5, 6, 7])]);
        let mut reader = OggOpusReader::new(&stream[..]);
        assert_eq!(reader.next_packet().unwrap().unwrap(), vec![1, 2, 3]);
        assert_eq!(reader.next_packet().unwrap().unwrap(), vec![4, 5, 6, 7]);
        assert!(reader.next_packet().unwrap().is_none());
        assert_eq!(reader.pre_skip(), 312);
    }

    #[test]
    fn a_packet_split_across_pages_is_rejoined() {
        let stream =
            stream_with_audio([page(0, 2, &[255], &[7u8; 255]), page(1, 3, &[5], &[9u8; 5])]);
        let mut reader = OggOpusReader::new(&stream[..]);
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.len(), 260);
        assert_eq!(&packet[..255], &[7u8; 255]);
        assert_eq!(&packet[255..], &[9u8; 5]);
    }

    #[test]
    fn packets_are_read_one_page_at_a_time() {
        let stream = stream_with_audio([page(0, 2, &[1, 1, 1], &[1, 2, 3])]);
        let mut reader = OggOpusReader::new(&stream[..]);
        for expected in [1u8, 2, 3] {
            assert_eq!(reader.next_packet().unwrap().unwrap(), vec![expected]);
        }
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn a_stream_that_is_not_ogg_is_refused() {
        assert!(OggOpusReader::new(&b"not an ogg stream at all"[..]).next_packet().is_err());
    }

    #[test]
    fn an_empty_stream_ends_immediately() {
        assert!(OggOpusReader::new(&[][..]).next_packet().unwrap().is_none());
    }

    #[test]
    fn a_truncated_page_is_rejected() {
        let head = opus_head(0);
        let mut stream = page(2, 0, &[head.len() as u8], &head);
        stream.extend_from_slice(&page(0, 1, &[4], &[1, 2, 3, 4])[..20]);
        assert_eq!(
            OggOpusReader::new(&stream[..]).next_packet().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn a_packet_missing_its_continuation_is_rejected() {
        let stream = stream_with_audio([page(0, 2, &[255], &[7; 255])]);
        assert_eq!(
            OggOpusReader::new(stream.as_slice()).next_packet().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn caller_storage_is_reused_across_headers_pages_and_eof() {
        let stream = stream_with_audio([
            page(0, 2, &[255], &[7; 255]),
            page(1, 3, &[5, 3], &[9, 9, 9, 9, 9, 1, 2, 3]),
        ]);
        let mut reader = OggOpusReader::new(stream.as_slice());
        let mut packet = Vec::with_capacity(1024);
        let storage = packet.as_ptr();
        assert!(reader.next_packet_into(&mut packet).unwrap());
        assert_eq!((packet.len(), packet.as_ptr()), (260, storage));
        assert!(reader.next_packet_into(&mut packet).unwrap());
        assert_eq!((packet.as_slice(), packet.as_ptr()), (&[1, 2, 3][..], storage));
        assert!(!reader.next_packet_into(&mut packet).unwrap());
        assert!(packet.is_empty() && packet.as_ptr() == storage);
    }

    #[test]
    fn non_opus_and_multistream_headers_are_rejected() {
        assert!(OggOpusReader::new(page(2, 0, &[19], &[0; 19]).as_slice()).next_packet().is_err());
        let mut head = opus_head(0);
        (head[9], head[18]) = (6, 1);
        assert!(OggOpusReader::new(page(2, 0, &[19], &head).as_slice()).next_packet().is_err());
    }

    #[test]
    fn many_small_pages_survive_deferred_compaction() {
        let stream = stream_with_audio((2..2002).map(|seq| page(0, seq, &[3], &[1, 2, 3])));
        let mut reader = OggOpusReader::new(stream.as_slice());
        let mut packet = Vec::new();
        let mut count = 0;
        while reader.next_packet_into(&mut packet).unwrap() {
            assert_eq!(packet, [1, 2, 3]);
            count += 1;
        }
        assert_eq!(count, 2000);
    }

    #[test]
    fn large_opus_tags_cover_art_over_one_mib_is_stream_skipped() {
        let head = opus_head(312);
        let mut stream = page(2, 0, &[head.len() as u8], &head);
        let mut tags = b"OpusTags".to_vec();
        tags.resize((3 << 19) + 17, b'A');
        let mut all_segs = vec![255u8; tags.len() / 255];
        all_segs.push((tags.len() % 255) as u8);
        let (mut offset, mut seq) = (0usize, 1u32);
        for segs in all_segs.chunks(255) {
            let len: usize = segs.iter().map(|&s| usize::from(s)).sum();
            stream.extend_from_slice(&page(
                u8::from(seq > 1),
                seq,
                segs,
                &tags[offset..offset + len],
            ));
            offset += len;
            seq += 1;
        }
        stream.extend_from_slice(&page(4, seq, &[3], &[9, 8, 7]));
        let mut reader = OggOpusReader::new(stream.as_slice());
        assert_eq!(reader.next_packet().unwrap().unwrap(), vec![9, 8, 7]);
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn mismatched_page_continuation_flags_are_rejected() {
        let orphan_cont = stream_with_audio([page(1, 2, &[4], &[1, 2, 3, 4])]);
        let missing_cont =
            stream_with_audio([page(0, 2, &[255], &[7; 255]), page(0, 3, &[5], &[9; 5])]);
        for bad in [orphan_cont, missing_cont] {
            assert_eq!(
                OggOpusReader::new(bad.as_slice()).next_packet().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
