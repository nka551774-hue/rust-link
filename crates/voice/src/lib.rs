//! Discord voice gateway v8, DAVE v1 negotiation, and RTP-size AEAD UDP transport.

mod gateway;
mod transport;
mod validate;

pub use gateway::{connect, DiscordConnection};
pub use rustlink_dave::MAX_PROTOCOL_VERSION as MAX_DAVE_PROTOCOL_VERSION;
pub use transport::{Mode, Transport};
pub use validate::{valid_id, validate};

pub const GATEWAY_VERSION: u32 = 8;

use std::sync::Arc;

use futures_util::future::BoxFuture;
use rustlink_protocol::VoiceState;

/// Why a voice connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    pub code: u16,
    pub reason: String,
    pub by_remote: bool,
}

/// Errors encountered while connecting or transmitting Discord voice audio.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("{0}")]
    Invalid(String),
    #[error("voice is disconnected")]
    NotReady,
    #[error("voice gateway: {0}")]
    Gateway(String),
    #[error("voice encryption: {0}")]
    Crypto(String),
    #[error("voice socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("voice connection closed: {code} {reason}")]
    Closed { code: u16, reason: String, by_remote: bool },
}

/// Sink for outgoing Opus frames and speaking state.
pub trait Connection: Send + Sync {
    fn write(&self, payload: &[u8]) -> Result<(), VoiceError>;
    fn speaking(&self, on: bool) -> Result<(), VoiceError>;
    fn connected(&self) -> bool;
    fn ping(&self) -> i64;
    fn close(&self);
}

/// Called once when a connection ends, whichever side ended it.
pub type CloseHandler = Box<dyn Fn(Closed) + Send + Sync>;

/// Opens voice connections.
pub trait Factory: Send + Sync {
    fn connect(
        &self,
        user: String,
        guild: String,
        state: VoiceState,
        on_close: CloseHandler,
    ) -> BoxFuture<'static, Result<Arc<dyn Connection>, VoiceError>>;
}

/// Production Discord voice gateway factory.
pub struct Discord;

impl Factory for Discord {
    fn connect(
        &self,
        user: String,
        guild: String,
        state: VoiceState,
        on_close: CloseHandler,
    ) -> BoxFuture<'static, Result<Arc<dyn Connection>, VoiceError>> {
        Box::pin(async move {
            Ok(gateway::connect(&user, &guild, &state, on_close).await? as Arc<dyn Connection>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_connection_names_both_the_code_and_the_side_that_closed_it() {
        let closed = Closed { code: 4014, reason: "disconnected".into(), by_remote: true };
        assert!(closed.by_remote && closed.code == 4014);
    }

    #[test]
    fn a_connection_with_nothing_to_send_says_so() {
        assert_eq!(VoiceError::NotReady.to_string(), "voice is disconnected");
    }
}
