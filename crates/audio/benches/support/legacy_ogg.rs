//! Frozen Rustlink 0.4.0 Ogg reader, retained only as a reproducible benchmark
//! baseline. This is the pre-optimization implementation (tests omitted), not
//! a production parser: its original allocation and EOF behavior are preserved.

use std::io::{self, Read};

const PAGE_HEADER: usize = 27;
const MAX_SEGMENTS: usize = 255;
const MAX_PACKET: usize = 1 << 20;

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
    partial: Vec<u8>,
    page: Option<PageState>,
    packets: u64,
    pre_skip: u32,
    drop_samples: u32,
    ended: bool,
}

impl<R: Read> OggOpusReader<R> {
    pub fn new(inner: R) -> Self {
        OggOpusReader {
            inner,
            buffer: Vec::new(),
            consumed: 0,
            partial: Vec::new(),
            page: None,
            packets: 0,
            pre_skip: 0,
            drop_samples: 0,
            ended: false,
        }
    }

    pub fn pre_skip(&self) -> u32 {
        self.pre_skip
    }

    pub fn next_packet(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let Some(packet) = self.next_raw_packet()? else {
                return Ok(None);
            };
            self.packets += 1;
            match self.packets {
                1 => {
                    self.pre_skip = read_pre_skip(&packet);
                    self.drop_samples = self.pre_skip;
                }
                2 => {}
                _ => return Ok(Some(packet)),
            }
        }
    }

    fn next_raw_packet(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut packet = std::mem::take(&mut self.partial);
        loop {
            if self.page.is_none() {
                if !self.have_page()? {
                    return Ok(if packet.is_empty() { None } else { Some(packet) });
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
                packet.extend_from_slice(&self.buffer[page.offset..end]);
                page.offset = end;
                if packet.len() > MAX_PACKET {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "ogg packet too large"));
                }
                if size < 255 {
                    self.consumed = page.offset;
                    if page.next_segment >= page.count {
                        self.page = None;
                    }
                    return Ok(Some(packet));
                }
            }
            self.consumed = page.offset;
            self.page = None;
        }
    }

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
                return Ok(false);
            }
        }
    }

    fn read_page_header(&self) -> io::Result<PageState> {
        let available = self.buffer.len() - self.consumed;
        if available < PAGE_HEADER {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated ogg page"));
        }
        let segments = self.buffer[self.consumed + 26] as usize;
        if segments > MAX_SEGMENTS {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "ogg segment table too long"));
        }
        let table_start = self.consumed + PAGE_HEADER;
        let payload_start = table_start + segments;
        Ok(PageState {
            table: table_start,
            count: segments,
            next_segment: 0,
            offset: payload_start,
        })
    }

    fn compact(&mut self) {
        if self.consumed > 0 {
            self.buffer.drain(..self.consumed);
            self.consumed = 0;
        }
    }

    fn fill(&mut self) -> io::Result<bool> {
        const CHUNK: usize = 16 * 1024;
        let start = self.buffer.len();
        self.buffer.resize(start + CHUNK, 0);
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
}

fn read_pre_skip(packet: &[u8]) -> u32 {
    if packet.len() < 12 || &packet[0..8] != b"OpusHead" {
        return 0;
    }
    u16::from_le_bytes([packet[10], packet[11]]) as u32
}
