//! Discord DAVE protocol v1: MLS membership, transitions and Opus encryption.
//! Protocol reference: <https://daveprotocol.com/>.

mod media;
mod session;

pub use session::{Outbound, Session, MAX_PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("DAVE: {0}")]
    Protocol(&'static str),
    #[error("DAVE encryption is not ready")]
    NotReady,
}

pub type Result<T> = std::result::Result<T, Error>;

fn check<T, E>(result: std::result::Result<T, E>, context: &'static str) -> Result<T> {
    result.map_err(|_| Error::Protocol(context))
}
