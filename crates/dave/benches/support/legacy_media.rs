//! 0.4.0 baseline DAVE v1 Opus sender (allocating per-frame baseline for benchmarks).
use aes_gcm::{
    aead::{AeadInPlace, KeyInit},
    Aes128Gcm, Nonce,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::{Error, Result};

pub(crate) struct MediaSender {
    next_secret: Zeroizing<Vec<u8>>,
    key: Zeroizing<[u8; 16]>,
    next_generation: u64,
    frame: u64,
}

impl MediaSender {
    pub(crate) fn new(secret: Vec<u8>) -> Self {
        Self {
            next_secret: Zeroizing::new(secret),
            key: Zeroizing::new([0; 16]),
            next_generation: 0,
            frame: 0,
        }
    }

    pub(crate) fn encrypt(&mut self, opus: &[u8]) -> Result<Vec<u8>> {
        self.frame = self.frame.checked_add(1).ok_or(Error::Protocol("media counter exhausted"))?;
        let generation = self.frame >> 24;
        if generation > u32::MAX as u64 {
            return Err(Error::Protocol("media ratchet exhausted"));
        }
        while self.next_generation <= generation {
            let g = self.next_generation as u32;
            self.key.copy_from_slice(&expand(&self.next_secret, b"key", g, 16));
            self.next_secret = expand(&self.next_secret, b"secret", g, 32);
            self.next_generation += 1;
        }
        let truncated = self.frame as u32;
        let mut nonce = [0u8; 12];
        nonce[8..].copy_from_slice(&truncated.to_le_bytes());
        let cipher = Aes128Gcm::new_from_slice(self.key.as_ref())
            .map_err(|_| Error::Protocol("invalid media key"))?;
        let mut frame = opus.to_vec();
        let tag = cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &[], &mut frame)
            .map_err(|_| Error::Protocol("media encryption failed"))?;
        frame.extend_from_slice(&tag[..8]);
        let (mut v, start) = (truncated, frame.len());
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                frame.push(byte);
                break;
            }
            frame.push(byte | 0x80);
        }
        let footer_len = (frame.len() - start) as u8 + 11;
        frame.extend_from_slice(&[footer_len, 0xfa, 0xfa]);
        Ok(frame)
    }
}

fn expand(secret: &[u8], label: &[u8], generation: u32, length: u16) -> Zeroizing<Vec<u8>> {
    let mut info = Vec::with_capacity(32);
    info.extend_from_slice(&length.to_be_bytes());
    info.push((8 + label.len()) as u8);
    info.extend_from_slice(b"MLS 1.0 ");
    info.extend_from_slice(label);
    info.push(4);
    info.extend_from_slice(&generation.to_be_bytes());
    info.push(1);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).unwrap();
    mac.update(&info);
    let block = Zeroizing::new(mac.finalize().into_bytes().to_vec());
    Zeroizing::new(block[..length as usize].to_vec())
}
