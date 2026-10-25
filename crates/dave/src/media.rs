//! DAVE v1 Opus sender: RFC 9420 sender ratchet and an eight-byte GCM tag.
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
    cipher: Option<Box<Aes128Gcm>>,
    next_generation: u64,
    frame: u64,
}

impl MediaSender {
    pub(crate) fn new(secret: Vec<u8>) -> Self {
        Self { next_secret: Zeroizing::new(secret), cipher: None, next_generation: 0, frame: 0 }
    }

    pub(crate) fn encrypt(&mut self, opus: &[u8]) -> Result<Vec<u8>> {
        let mut frame = Vec::with_capacity(opus.len() + 16);
        frame.extend_from_slice(opus);
        self.encrypt_in_place(&mut frame)?;
        Ok(frame)
    }

    /// Reuses the gateway's owned packet. A DAVE footer needs at most 16 bytes.
    pub(crate) fn encrypt_in_place(&mut self, frame: &mut Vec<u8>) -> Result<()> {
        // Increment before use, and never reuse a nonce even if sending fails.
        self.frame = self.frame.checked_add(1).ok_or(Error::Protocol("media counter exhausted"))?;
        let generation = self.frame >> 24;
        if generation > u32::MAX as u64 {
            return Err(Error::Protocol("media ratchet exhausted"));
        }
        while self.next_generation <= generation {
            let generation = self.next_generation as u32;
            let key = expand(&self.next_secret, b"key", generation, 16);
            self.cipher = Some(Box::new(
                Aes128Gcm::new_from_slice(&key)
                    .map_err(|_| Error::Protocol("invalid media key"))?,
            ));
            self.next_secret = expand(&self.next_secret, b"secret", generation, 32);
            self.next_generation += 1;
        }
        let truncated = self.frame as u32;
        let mut nonce = [0u8; 12];
        nonce[8..].copy_from_slice(&truncated.to_le_bytes());
        frame.reserve(16);
        let tag = self
            .cipher
            .as_ref()
            .ok_or(Error::NotReady)?
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &[], frame)
            .map_err(|_| Error::Protocol("media encryption failed"))?;
        frame.extend_from_slice(&tag[..8]);
        let mut value = truncated;
        let mut nonce_len = 0;
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            frame.push(byte);
            nonce_len += 1;
            if value == 0 {
                break;
            }
        }
        frame.extend_from_slice(&[8 + nonce_len + 1 + 2, 0xfa, 0xfa]);
        Ok(())
    }
}

// RFC 9420 ExpandWithLabel, using one HKDF-Expand block (all outputs <= 32).
// MLS variable-length vectors shorter than 64 bytes use a single length byte.
fn expand(secret: &[u8], label: &[u8], generation: u32, length: u16) -> Zeroizing<Vec<u8>> {
    let mut info = Vec::with_capacity(32);
    info.extend_from_slice(&length.to_be_bytes());
    info.push((8 + label.len()) as u8);
    info.extend_from_slice(b"MLS 1.0 ");
    info.extend_from_slice(label);
    info.push(4);
    info.extend_from_slice(&generation.to_be_bytes());
    info.push(1); // first HKDF block
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&info);
    let block = Zeroizing::new(mac.finalize().into_bytes().to_vec());
    Zeroizing::new(block[..length as usize].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratchet_matches_the_independent_dave_reference_vector() {
        let secret = [206, 221, 97, 177, 184, 161, 202, 105, 4, 101, 84, 40, 44, 247, 11, 123];
        assert_eq!(
            &*expand(&secret, b"key", 0, 16),
            &[117, 48, 249, 169, 148, 94, 45, 46, 6, 208, 101, 31, 123, 42, 134, 75]
        );
    }

    #[test]
    fn nonce_wrap_keeps_advancing_the_key_generation() {
        let mut sender = MediaSender::new(vec![42; 16]);
        let first = sender.encrypt(b"opus").unwrap();
        sender.frame = u32::MAX as u64 + 1;
        let wrapped = sender.encrypt(b"opus").unwrap();
        assert_eq!(sender.next_generation, 257);
        assert_eq!(&first[12..], &wrapped[12..]); // nonce 1, footer
        assert_ne!(&first[..12], &wrapped[..12]); // different key
    }

    #[test]
    fn generation_changes_at_24_bits_and_overflow_fails_closed() {
        let mut sender = MediaSender::new(vec![42; 16]);
        sender.frame = (1 << 24) - 2;
        let old = sender.encrypt(b"opus").unwrap();
        assert_eq!(sender.next_generation, 1);
        let next = sender.encrypt(b"opus").unwrap();
        assert_eq!(sender.next_generation, 2);
        assert_ne!(&old[..12], &next[..12]);
        sender.frame = u64::MAX;
        assert!(sender.encrypt(b"opus").is_err());
    }

    #[test]
    fn owned_frames_match_borrowed_frames_and_reuse_capacity() {
        let mut borrowed = MediaSender::new(vec![42; 16]);
        let mut owned = MediaSender::new(vec![42; 16]);
        let mut frame = Vec::with_capacity(1500);
        let pointer = frame.as_ptr();
        for size in [3, 320, 1275, 0, 960] {
            frame.clear();
            frame.resize(size, 123);
            let expected = borrowed.encrypt(&frame).unwrap();
            owned.encrypt_in_place(&mut frame).unwrap();
            assert_eq!(frame, expected);
            assert_eq!(frame.as_ptr(), pointer);
        }
    }
}
