//! Streaming Ogg Opus packet reader. Packet boundaries may cross pages; the
//! caller can reuse one packet buffer for the complete lifetime of a stream.

use std::io::{self, Read};

const PAGE_HEADER: usize = 27;
/// Reject damaged packets before their storage can grow without bound.
const MAX_PACKET: usize = 1 << 20;
/// `OpusTags` may carry embedded cover art larger than an audio packet.
const MAX_TAGS_PACKET: usize = 64 << 20;
const READ_CHUNK: usize = 16 * 1024;

struct PageState {
    table: usize,
    count: usize,
    next_segment: usize,
    offset: usize,
    granule: u64,
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
    /// Signed Q8 dB gain from OpusHead, applied by the decoder.
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

    /// Returns the next audio packet, or `None` at EOF. Streaming consumers
    /// should use `next_packet_into` to reuse their storage.
    pub fn next_packet(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut packet = Vec::new();
        self.next_packet_into(&mut packet).map(|found| found.then_some(packet))
    }

    /// Reuses the caller's packet storage across headers, pages and packets.
    /// The buffer is cleared before every packet and at EOF.
    pub fn next_packet_into(&mut self, packet: &mut Vec<u8>) -> io::Result<bool> {
        packet.clear();
        self.append_next_packet(packet)
    }

    /// Appends the next audio packet onto `packet` without clearing prior bytes,
    /// allowing a page decoder to stage a full page in one contiguous buffer.
    #[inline]
    pub(crate) fn append_next_packet(&mut self, packet: &mut Vec<u8>) -> io::Result<bool> {
        if self.packets >= 2 {
            return self.append_raw_packet(packet);
        }
        self.append_first_audio_packet(packet)
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
                if magic_len < 8 {
                    let take = (8 - magic_len).min(span.len());
                    magic[magic_len..magic_len + take].copy_from_slice(&span[..take]);
                    magic_len += take;
                }
            },
        )?;
        if !found || magic_len < 8 || &magic != b"OpusTags" {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "missing OpusTags"));
        }
        Ok(())
    }

    fn append_raw_packet(&mut self, packet: &mut Vec<u8>) -> io::Result<bool> {
        self.read_laced_packet(MAX_PACKET, "ogg packet too large", true, false, |span| {
            packet.extend_from_slice(span);
        })
    }

    /// Reads one complete laced packet across page boundaries, passing each
    /// page's contiguous payload span to `on_span`.
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
                    let msg =
                        if require_packet { "missing OpusTags" } else { "truncated ogg packet" };
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, msg));
                }
                self.page = Some(self.read_page_header()?);
            }
            let page = self.page.as_mut().expect("a page was just read");
            let segs = &self.buffer[page.table + page.next_segment..page.table + page.count];
            let mut consumed_segs = 0usize;
            let mut chunk_len = 0usize;
            let mut done = false;
            for &seg in segs {
                consumed_segs += 1;
                chunk_len += usize::from(seg);
                if seg < 255 {
                    done = true;
                    break;
                }
            }
            let start = page.offset;
            let end = start + chunk_len;
            if end > self.filled {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated ogg page"));
            }
            total = total.saturating_add(chunk_len);
            if total > limit {
                return Err(io::Error::new(io::ErrorKind::InvalidData, too_large_msg));
            }
            on_span(&self.buffer[start..end]);
            page.next_segment += consumed_segs;
            page.offset = end;
            if done {
                self.consumed = end;
                // A page may finish one packet and start another. Its granule
                // belongs to the last completed packet, even when that packet
                // is not the page's final lacing segment.
                if page.next_segment == page.last_complete {
                    if record_granule {
                        self.packet_page_end = Some((page.granule, page.end_of_stream));
                    }
                    if page.end_of_stream {
                        self.ended = true;
                    }
                }
                if page.next_segment >= page.count {
                    self.page = None;
                }
                return Ok(true);
            }
            if page.end_of_stream {
                self.ended = true;
            }
            started |= consumed_segs > 0;
            self.consumed = end;
            self.page = None;
        }
    }

    /// Buffer one complete page before exposing its segment table.
    fn have_page(&mut self) -> io::Result<bool> {
        if self.ended {
            return Ok(false);
        }
        loop {
            self.compact();
            let unconsumed = &self.buffer[self.consumed..self.filled];
            let available = unconsumed.len();
            if available >= 4 && &unconsumed[..4] != b"OggS" {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not an ogg page"));
            }
            if available >= 5 && unconsumed[4] != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported ogg page version",
                ));
            }
            if available >= PAGE_HEADER {
                let segments = usize::from(unconsumed[26]);
                let total = PAGE_HEADER + segments;
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
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated ogg page"))
                };
            }
        }
    }

    fn read_page_header(&mut self) -> io::Result<PageState> {
        let Some(unconsumed) = self.buffer.get(self.consumed..self.filled) else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated ogg page"));
        };
        if unconsumed.len() < PAGE_HEADER {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated ogg page"));
        }
        let serial = u32::from_le_bytes(unconsumed[14..18].try_into().unwrap());
        if let Some(expected) = self.serial {
            if serial != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "multiplexed ogg stream requires ffmpeg",
                ));
            }
        } else {
            self.serial = Some(serial);
        }
        let count = usize::from(unconsumed[26]);
        let table = self.consumed + PAGE_HEADER;
        // No compaction takes place while a page is open, so the table can be
        // read from the input buffer without copying it into another array.
        let granule = u64::from_le_bytes(unconsumed[6..14].try_into().unwrap());
        let end_of_stream = unconsumed[5] & 4 != 0;
        let segments = &unconsumed[PAGE_HEADER..PAGE_HEADER + count];
        let last_complete = segments.iter().rposition(|&size| size < 255).map_or(0, |i| i + 1);
        if end_of_stream && segments.last() == Some(&255) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unfinished final ogg packet",
            ));
        }
        Ok(PageState {
            table,
            count,
            next_segment: 0,
            offset: table + count,
            granule,
            end_of_stream,
            last_complete,
        })
    }

    fn compact(&mut self) {
        // FFmpeg commonly writes small pages. Moving the unread tail at each
        // page made parsing proportional to page count times read chunk size.
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

    /// EOS granule for the packet most recently returned. It counts decoded
    /// 48 kHz samples including pre-skip, and can trim the final packet's padding.
    pub fn packet_end_granule(&self) -> Option<u64> {
        self.packet_page_end.and_then(|(granule, eos)| eos.then_some(granule))
    }

    /// Timing metadata after the last packet completed on a page. A trailing
    /// partial packet belongs to the later page on which it completes.
    pub(crate) fn packet_page_end(&self) -> Option<(u64, bool)> {
        self.packet_page_end
    }
}

/// The native decoder accepts mono/stereo mapping family zero. Other layouts
/// require a multistream decoder and must not silently play as stereo.
fn read_head(packet: &[u8]) -> io::Result<(u32, i16)> {
    if packet.len() < 19 || !packet.starts_with(b"OpusHead") || packet[8] > 15 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid OpusHead"));
    }
    if !(1..=2).contains(&packet[9]) || packet[18] != 0 {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported Opus channel mapping"));
    }
    Ok((
        u16::from_le_bytes([packet[10], packet[11]]) as u32,
        i16::from_le_bytes([packet[16], packet[17]]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(header_type: u8, sequence: u32, segments: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"OggS");
        out.push(0);
        out.push(header_type);
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
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(2);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        head
    }

    fn stream_with_audio(pages: Vec<Vec<u8>>) -> Vec<u8> {
        let head = opus_head(312);
        let tags = b"OpusTags\x00\x00\x00\x00".to_vec();
        let mut stream = page(2, 0, &[head.len() as u8], &head);
        stream.extend_from_slice(&page(0, 1, &[tags.len() as u8], &tags));
        for entry in pages {
            stream.extend_from_slice(&entry);
        }
        stream
    }

    #[test]
    fn the_headers_are_skipped_and_the_audio_packets_come_back_in_order() {
        let stream = stream_with_audio(vec![page(0, 2, &[3, 4], &[1, 2, 3, 4, 5, 6, 7])]);
        let mut reader = OggOpusReader::new(&stream[..]);
        assert_eq!(reader.next_packet().unwrap().unwrap(), vec![1, 2, 3]);
        assert_eq!(reader.next_packet().unwrap().unwrap(), vec![4, 5, 6, 7]);
        assert!(reader.next_packet().unwrap().is_none());
        assert_eq!(reader.pre_skip(), 312);
    }

    #[test]
    fn a_packet_split_across_pages_is_rejoined() {
        let stream =
            stream_with_audio(vec![page(0, 2, &[255], &[7u8; 255]), page(1, 3, &[5], &[9u8; 5])]);
        let mut reader = OggOpusReader::new(&stream[..]);
        let packet = reader.next_packet().unwrap().unwrap();
        assert_eq!(packet.len(), 260);
        assert_eq!(&packet[..255], &[7u8; 255]);
        assert_eq!(&packet[255..], &[9u8; 5]);
    }

    #[test]
    fn packets_are_read_one_page_at_a_time() {
        let stream = stream_with_audio(vec![page(0, 2, &[1, 1, 1], &[1, 2, 3])]);
        let mut reader = OggOpusReader::new(&stream[..]);
        assert_eq!(reader.next_packet().unwrap().unwrap(), vec![1]);
        assert_eq!(reader.next_packet().unwrap().unwrap(), vec![2]);
        assert_eq!(reader.next_packet().unwrap().unwrap(), vec![3]);
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn a_stream_that_is_not_ogg_is_refused() {
        let mut reader = OggOpusReader::new(&b"not an ogg stream at all"[..]);
        assert!(reader.next_packet().is_err());
    }

    #[test]
    fn an_empty_stream_ends_immediately() {
        let mut reader = OggOpusReader::new(&[][..]);
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn a_truncated_page_is_rejected() {
        let head = opus_head(0);
        let mut stream = page(2, 0, &[head.len() as u8], &head);
        stream.extend_from_slice(&page(0, 1, &[4], &[1, 2, 3, 4])[..20]);
        let mut reader = OggOpusReader::new(&stream[..]);
        assert_eq!(reader.next_packet().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_packet_missing_its_continuation_is_rejected() {
        let stream = stream_with_audio(vec![page(0, 2, &[255], &[7; 255])]);
        let mut reader = OggOpusReader::new(stream.as_slice());
        assert_eq!(reader.next_packet().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn caller_storage_is_reused_across_headers_pages_and_eof() {
        let stream = stream_with_audio(vec![
            page(0, 2, &[255], &[7; 255]),
            page(1, 3, &[5, 3], &[9, 9, 9, 9, 9, 1, 2, 3]),
        ]);
        let mut reader = OggOpusReader::new(stream.as_slice());
        let mut packet = Vec::with_capacity(1024);
        let storage = packet.as_ptr();
        assert!(reader.next_packet_into(&mut packet).unwrap());
        assert_eq!(packet.len(), 260);
        assert_eq!(packet.as_ptr(), storage);
        assert!(reader.next_packet_into(&mut packet).unwrap());
        assert_eq!(packet, [1, 2, 3]);
        assert_eq!(packet.as_ptr(), storage);
        assert!(!reader.next_packet_into(&mut packet).unwrap());
        assert!(packet.is_empty());
        assert_eq!(packet.as_ptr(), storage);
    }

    #[test]
    fn non_opus_and_multistream_headers_are_rejected() {
        let stream = page(2, 0, &[19], &[0; 19]);
        assert!(OggOpusReader::new(stream.as_slice()).next_packet().is_err());
        let mut head = opus_head(0);
        head[9] = 6;
        head[18] = 1;
        let stream = page(2, 0, &[19], &head);
        assert!(OggOpusReader::new(stream.as_slice()).next_packet().is_err());
    }

    #[test]
    fn many_small_pages_survive_deferred_compaction() {
        let pages = (2..2002).map(|sequence| page(0, sequence, &[3], &[1, 2, 3])).collect();
        let stream = stream_with_audio(pages);
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
        // Build a 1.5 MiB OpusTags packet spanning ~25 full Ogg pages.
        let mut tags = b"OpusTags".to_vec();
        tags.resize((3 << 19) + 17, b'A');
        let mut rest = tags.as_slice();
        let mut seq = 1u32;
        while !rest.is_empty() {
            let max_page = 255 * 255;
            if rest.len() > max_page {
                stream.extend_from_slice(&page(
                    if seq == 1 { 0 } else { 1 },
                    seq,
                    &[255; 255],
                    &rest[..max_page],
                ));
                rest = &rest[max_page..];
            } else {
                let mut segs = vec![255u8; rest.len() / 255];
                segs.push((rest.len() % 255) as u8);
                stream.extend_from_slice(&page(if seq == 1 { 0 } else { 1 }, seq, &segs, rest));
                rest = &[];
            }
            seq += 1;
        }
        stream.extend_from_slice(&page(4, seq, &[3], &[9, 8, 7]));
        let mut reader = OggOpusReader::new(stream.as_slice());
        assert_eq!(reader.next_packet().unwrap().unwrap(), vec![9, 8, 7]);
        assert!(reader.next_packet().unwrap().is_none());
    }
}
