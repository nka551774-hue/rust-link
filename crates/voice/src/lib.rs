//! Discord voice.
//!
//! Voice gateway v8, DAVE negotiation, and authenticated RTP-size UDP transport.
//! The bot supplies Discord voice credentials through Rustlink API v1.

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
    /// Whether the far end closed it, which is the difference between a
    /// connection the node dropped on purpose and one that died.
    pub by_remote: bool,
}

/// What went wrong on the way to sending audio.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    /// The voice state a client sent cannot be acted on.
    #[error("{0}")]
    Invalid(String),
    /// There is nothing to write to: the session is not described yet, or the
    /// connection is gone.
    #[error("voice is disconnected")]
    NotReady,
    /// The voice gateway refused or broke the handshake.
    #[error("voice gateway: {0}")]
    Gateway(String),
    /// A payload could not be encrypted.
    #[error("voice encryption: {0}")]
    Crypto(String),
    /// The voice socket failed.
    #[error("voice socket: {0}")]
    Io(#[from] std::io::Error),
    /// The gateway closed the connection.
    #[error("voice connection closed: {code} {reason}")]
    Closed { code: u16, reason: String, by_remote: bool },
}

/// Where a player's audio goes.
///
/// The trait exists so that a test can hand a player somewhere harmless to write
/// instead of opening a socket to Discord.
pub trait Connection: Send + Sync {
    /// Frames and encrypts one Opus packet and sends it.
    fn write(&self, payload: &[u8]) -> Result<(), VoiceError>;
    /// Tells Discord whether the node is speaking, which has to be set before
    /// the first frame.
    fn speaking(&self, on: bool) -> Result<(), VoiceError>;
    /// Whether audio written now would reach Discord.
    fn connected(&self) -> bool;
    /// The last heartbeat round trip in milliseconds, or -1 when there is none.
    fn ping(&self) -> i64;
    /// Ends the connection. Calling it twice does nothing the second time.
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

/// The real Discord voice gateway.
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
            let connection = gateway::connect(&user, &guild, &state, on_close).await?;
            Ok(connection as Arc<dyn Connection>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_connection_names_both_the_code_and_the_side_that_closed_it() {
        let closed = Closed { code: 4014, reason: "disconnected".into(), by_remote: true };
        assert!(closed.by_remote);
        assert_eq!(closed.code, 4014);
    }

    #[test]
    fn a_connection_with_nothing_to_send_says_so() {
        assert_eq!(VoiceError::NotReady.to_string(), "voice is disconnected");
    }
}
