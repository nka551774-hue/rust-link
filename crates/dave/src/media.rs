//! DAVE v1 Opus sender: RFC 9420 sender ratchet and 8-byte AES-128-GCM tag.
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
        Self {
            cipher: Box::new(derive_cipher(&secret, 0)),
            next_secret: expand(&secret, b"secret", 0, 32),
            next_generation: 1,
            frame: 0,
        }
    }

    pub fn encrypt(&mut self, opus: &[u8]) -> Result<Vec<u8>> {
        let mut frame = Vec::with_capacity(opus.len() + 16);
        frame.extend_from_slice(opus);
        self.encrypt_in_place(&mut frame)?;
        Ok(frame)
    }

    #[cold]
    fn advance_ratchet(&mut self, target: u64) -> Result<()> {
        let target_u32 =
            u32::try_from(target).map_err(|_| Error::Protocol("media ratchet exhausted"))?;
        while self.next_generation < target {
            let g = self.next_generation as u32;
            self.next_secret = expand(self.next_secret.as_slice(), b"secret", g, 32);
            self.next_generation += 1;
        }
        *self.cipher = derive_cipher(self.next_secret.as_slice(), target_u32);
        self.next_secret = expand(self.next_secret.as_slice(), b"secret", target_u32, 32);
        self.next_generation = target + 1;
        Ok(())
    }

    /// Encrypts an owned Opus packet in place; DAVE v1 appends at most 16 footer bytes.
    #[inline]
    pub fn encrypt_in_place(&mut self, frame: &mut Vec<u8>) -> Result<()> {
        self.encrypt_in_place_from(frame, 0)
    }

    /// Encrypts `frame[payload_offset..]` in place without touching `frame[..payload_offset]`,
    /// then appends the DAVE v1 supplemental footer (at most 16 bytes).
    #[inline]
    pub fn encrypt_in_place_from(
        &mut self,
        frame: &mut Vec<u8>,
        payload_offset: usize,
    ) -> Result<()> {
        if payload_offset > frame.len() {
            return Err(Error::Protocol("payload offset exceeds frame length"));
        }
        // Increment before use so a nonce is never reused even if sending fails.
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
        let (mut pos, mut v) = (8, truncated);
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                footer[pos] = byte;
                pos += 1;
                break;
            }
            footer[pos] = byte | 0x80;
            pos += 1;
        }
        footer[pos..pos + 3].copy_from_slice(&[(pos + 3) as u8, 0xfa, 0xfa]);
        frame.extend_from_slice(&footer[..pos + 3]);
        Ok(())
    }
}

#[inline]
fn derive_cipher(secret: &[u8], generation: u32) -> Aes128Gcm {
    let key = expand(secret, b"key", generation, 16);
    Aes128Gcm::new(key.first_chunk::<16>().unwrap().into())
}

// RFC 9420 ExpandWithLabel using one HKDF-Expand block (all outputs <= 32 bytes).
fn expand(secret: &[u8], label: &[u8], generation: u32, length: u16) -> Zeroizing<[u8; 32]> {
    debug_assert!(label.len() <= 15 && length <= 32);
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&length.to_be_bytes());
    mac.update(&[(8 + label.len()) as u8]);
    mac.update(b"MLS 1.0 ");
    mac.update(label);
    mac.update(&[4]);
    mac.update(&generation.to_be_bytes());
    mac.update(&[1]);
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
        assert_eq!(&first[12..], &wrapped[12..]);
        assert_ne!(&first[..12], &wrapped[..12]);
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
        let (mut borrowed, mut owned) =
            (MediaSender::new(vec![42; 16]), MediaSender::new(vec![42; 16]));
        let mut frame = Vec::with_capacity(1500);
        let ptr = frame.as_ptr();
        for size in [3, 320, 1275, 0, 960] {
            frame.clear();
            frame.resize(size, 123);
            let expected = borrowed.encrypt(&frame).unwrap();
            owned.encrypt_in_place(&mut frame).unwrap();
            assert_eq!(frame, expected);
            assert_eq!(frame.as_ptr(), ptr);
        }
    }

    #[test]
    fn offset_encryption_preserves_prefix_and_matches_standalone_frames() {
        let (mut standalone, mut offset_sender) =
            (MediaSender::new(vec![42; 16]), MediaSender::new(vec![42; 16]));
        assert_eq!(offset_sender.next_generation, 1);
        let mut frame = Vec::with_capacity(1500);
        let ptr = frame.as_ptr();
        let prefix = [0x80, 0x78, 0, 1, 0, 0, 3, 192, 0, 0, 0, 77];
        for size in [0, 3, 320, 960, 1275] {
            let payload: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(7)).collect();
            let expected = standalone.encrypt(&payload).unwrap();
            frame.clear();
            frame.extend_from_slice(&prefix);
            frame.extend_from_slice(&payload);
            offset_sender.encrypt_in_place_from(&mut frame, prefix.len()).unwrap();
            assert_eq!(&frame[..prefix.len()], &prefix);
            assert_eq!(&frame[prefix.len()..], expected.as_slice());
            assert_eq!(frame.as_ptr(), ptr);
        }
        frame.clear();
        frame.extend_from_slice(&[1, 2, 3]);
        assert!(offset_sender.encrypt_in_place_from(&mut frame, 4).is_err());
    }
}
