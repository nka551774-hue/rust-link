//! Streaming Ogg Opus packet reader. Packet boundaries may cross pages; the
//! caller can reuse one packet buffer for the complete lifetime of a stream.

use std::io::{self, Read};

const PAGE_HEADER: usize = 27;
/// Reject damaged packets before their storage can grow without bound.
const MAX_PACKET: usize = 1 << 20;
const READ_CHUNK: usize = 16 * 1024;

struct PageState {
    table: usize,
    count: usize,
    next_segment: usize,
    offset: usize,
}

pub struct OggOpusReader<R: Read> {
    inner: R,
    buffer: Vec<u8>,
    consumed: usize,
    page: Option<PageState>,
    packets: u64,
    pre_skip: u32,
    drop_samples: u32,
    /// Signed Q8 dB gain from OpusHead, applied by the decoder.
    output_gain: i16,
    ended: bool,
}

impl<R: Read> OggOpusReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            consumed: 0,
            page: None,
            packets: 0,
            pre_skip: 0,
            drop_samples: 0,
            output_gain: 0,
            ended: false,
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
        loop {
            if !self.next_raw_packet_into(packet)? {
                return Ok(false);
            }
            self.packets += 1;
            match self.packets {
                1 => {
                    let (pre_skip, output_gain) = read_head(packet)?;
                    self.pre_skip = pre_skip;
                    self.drop_samples = pre_skip;
                    self.output_gain = output_gain;
                }
                2 if packet.starts_with(b"OpusTags") => {}
                2 => return Err(io::Error::new(io::ErrorKind::InvalidData, "missing OpusTags")),
                _ => return Ok(true),
            }
        }
    }

    fn next_raw_packet_into(&mut self, packet: &mut Vec<u8>) -> io::Result<bool> {
        packet.clear();
        loop {
            if self.page.is_none() {
                if !self.have_page()? {
                    if packet.is_empty() {
                        return Ok(false);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated ogg packet",
                    ));
                }
                self.page = Some(self.read_page_header()?);
            }
            let page = self.page.as_mut().expect("a page was just read");
            while page.next_segment < page.count {
                let size = self.buffer[page.table + page.next_segment] as usize;
                page.next_segment += 1;
                let end = page.offset + size;
                if end > self.buffer.len() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated ogg page"));
                }
                if packet.len() + size > MAX_PACKET {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "ogg packet too large"));
                }
                packet.extend_from_slice(&self.buffer[page.offset..end]);
                page.offset = end;
                if size < 255 {
                    self.consumed = page.offset;
                    if page.next_segment >= page.count {
                        self.page = None;
                    }
                    return Ok(true);
                }
            }
            self.consumed = page.offset;
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
            let available = self.buffer.len() - self.consumed;
            if available >= 4 && &self.buffer[self.consumed..self.consumed + 4] != b"OggS" {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not an ogg page"));
            }
            if available >= PAGE_HEADER {
                let segments = self.buffer[self.consumed + 26] as usize;
                let total = PAGE_HEADER + segments;
                if available >= total {
                    let payload: usize = self.buffer
                        [self.consumed + PAGE_HEADER..self.consumed + total]
                        .iter()
                        .map(|n| *n as usize)
                        .sum();
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

    fn read_page_header(&self) -> io::Result<PageState> {
        if self.buffer.len() - self.consumed < PAGE_HEADER {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated ogg page"));
        }
        let count = self.buffer[self.consumed + 26] as usize;
        let table = self.consumed + PAGE_HEADER;
        // No compaction takes place while a page is open, so the table can be
        // read from the input buffer without copying it into another array.
        Ok(PageState { table, count, next_segment: 0, offset: table + count })
    }

    fn compact(&mut self) {
        // FFmpeg commonly writes small pages. Moving the unread tail at each
        // page made parsing proportional to page count times read chunk size.
        if self.consumed > 0 && (self.consumed == self.buffer.len() || self.consumed >= READ_CHUNK)
        {
            self.buffer.drain(..self.consumed);
            self.consumed = 0;
        }
    }

    fn fill(&mut self) -> io::Result<bool> {
        let start = self.buffer.len();
        self.buffer.resize(start + READ_CHUNK, 0);
        match self.inner.read(&mut self.buffer[start..]) {
            Ok(read) => {
                self.buffer.truncate(start + read);
                Ok(read > 0)
            }
            Err(error) => {
                self.buffer.truncate(start);
                Err(error)
            }
        }
    }

    pub fn take_drop_samples(&mut self) -> u32 {
        std::mem::take(&mut self.drop_samples)
    }

    pub fn output_gain(&self) -> i16 {
        self.output_gain
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
}
