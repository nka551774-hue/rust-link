//! DAVE v1 Opus sender: RFC 9420 sender ratchet and an eight-byte GCM tag.
use aes_gcm::{
    aead::{AeadInPlace, KeyInit},
    Aes128Gcm, Nonce,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::{Error, Result};

pub struct MediaSender {
    next_secret: Zeroizing<[u8; 32]>,
    cipher: Box<Aes128Gcm>,
    next_generation: u64,
    frame: u64,
}

impl MediaSender {
    pub fn new(secret: Vec<u8>) -> Self {
        let secret = Zeroizing::new(secret);
        assert!(secret.len() <= 32, "MLS exporter secret exceeds 32 bytes");
        let key = expand(&secret, b"key", 0, 16);
        let array: &[u8; 16] = key[..16].try_into().unwrap();
        let cipher = Box::new(Aes128Gcm::new(array.into()));
        let next_secret = expand(&secret, b"secret", 0, 32);
        Self { next_secret, cipher, next_generation: 1, frame: 0 }
    }

    pub fn encrypt(&mut self, opus: &[u8]) -> Result<Vec<u8>> {
        let mut frame = Vec::with_capacity(opus.len() + 16);
        frame.extend_from_slice(opus);
        self.encrypt_in_place(&mut frame)?;
        Ok(frame)
    }

    #[cold]
    fn advance_ratchet(&mut self, target_generation: u64) -> Result<()> {
        if target_generation > u32::MAX as u64 {
            return Err(Error::Protocol("media ratchet exhausted"));
        }
        while self.next_generation <= target_generation {
            let generation = self.next_generation as u32;
            if self.next_generation == target_generation {
                let key = expand(self.next_secret.as_slice(), b"key", generation, 16);
                let array: &[u8; 16] = key[..16].try_into().unwrap();
                *self.cipher = Aes128Gcm::new(array.into());
            }
            self.next_secret = expand(self.next_secret.as_slice(), b"secret", generation, 32);
            self.next_generation += 1;
        }
        Ok(())
    }

    /// Reuses the gateway's owned packet. A DAVE footer needs at most 16 bytes.
    pub fn encrypt_in_place(&mut self, frame: &mut Vec<u8>) -> Result<()> {
        self.encrypt_in_place_from(frame, 0)
    }

    /// Encrypts `frame[payload_offset..]` in place without touching `frame[..payload_offset]`,
    /// then appends the DAVE v1 supplemental footer (at most 16 bytes).
    pub fn encrypt_in_place_from(
        &mut self,
        frame: &mut Vec<u8>,
        payload_offset: usize,
    ) -> Result<()> {
        if payload_offset > frame.len() {
            return Err(Error::Protocol("payload offset exceeds frame length"));
        }
        // Increment before use, and never reuse a nonce even if sending fails.
        self.frame = self.frame.checked_add(1).ok_or(Error::Protocol("media counter exhausted"))?;
        let generation = self.frame >> 24;
        if self.next_generation <= generation {
            self.advance_ratchet(generation)?;
        }
        let truncated = self.frame as u32;
        let mut nonce = [0u8; 12];
        nonce[8..].copy_from_slice(&truncated.to_le_bytes());
        frame.reserve(16);
        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &[], &mut frame[payload_offset..])
            .map_err(|_| Error::Protocol("media encryption failed"))?;
        let mut footer = [0u8; 16];
        footer[..8].copy_from_slice(&tag[..8]);
        let mut pos = 8usize;
        let mut value = truncated;
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            footer[pos] = byte;
            pos += 1;
            if value == 0 {
                break;
            }
        }
        let nonce_len = (pos - 8) as u8;
        footer[pos] = 8 + nonce_len + 1 + 2;
        footer[pos + 1] = 0xfa;
        footer[pos + 2] = 0xfa;
        frame.extend_from_slice(&footer[..pos + 3]);
        Ok(())
    }
}

// RFC 9420 ExpandWithLabel, using one HKDF-Expand block (all outputs <= 32).
// MLS variable-length vectors shorter than 64 bytes use a single length byte.
fn expand(secret: &[u8], label: &[u8], generation: u32, length: u16) -> Zeroizing<[u8; 32]> {
    debug_assert!(label.len() <= 15 && length <= 32);
    let mut info = [0u8; 32];
    info[..2].copy_from_slice(&length.to_be_bytes());
    info[2] = (8 + label.len()) as u8;
    info[3..11].copy_from_slice(b"MLS 1.0 ");
    let label_end = 11 + label.len();
    info[11..label_end].copy_from_slice(label);
    info[label_end] = 4;
    info[label_end + 1..label_end + 5].copy_from_slice(&generation.to_be_bytes());
    info[label_end + 5] = 1; // first HKDF block
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&info[..label_end + 6]);
    let mut out = Zeroizing::new([0u8; 32]);
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratchet_matches_the_independent_dave_reference_vector() {
        let secret = [206, 221, 97, 177, 184, 161, 202, 105, 4, 101, 84, 40, 44, 247, 11, 123];
        assert_eq!(
            &expand(&secret, b"key", 0, 16)[..16],
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

    #[test]
    fn offset_encryption_preserves_prefix_and_matches_standalone_frames() {
        let mut standalone = MediaSender::new(vec![42; 16]);
        let mut offset_sender = MediaSender::new(vec![42; 16]);
        assert_eq!(offset_sender.next_generation, 1);

        let mut frame = Vec::with_capacity(1500);
        let pointer = frame.as_ptr();
        let prefix = [0x80, 0x78, 0x00, 0x01, 0, 0, 3, 192, 0, 0, 0, 77];
        for size in [0, 3, 320, 960, 1275] {
            let payload: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(7)).collect();
            let expected = standalone.encrypt(&payload).unwrap();
            frame.clear();
            frame.extend_from_slice(&prefix);
            frame.extend_from_slice(&payload);
            offset_sender.encrypt_in_place_from(&mut frame, prefix.len()).unwrap();
            assert_eq!(&frame[..prefix.len()], &prefix);
            assert_eq!(&frame[prefix.len()..], expected.as_slice());
            assert_eq!(frame.as_ptr(), pointer);
        }

        frame.clear();
        frame.extend_from_slice(&[1, 2, 3]);
        assert!(offset_sender.encrypt_in_place_from(&mut frame, 4).is_err());
    }
}
