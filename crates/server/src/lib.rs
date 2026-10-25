//! The node: sessions, players, and the HTTP and WebSocket surface a client
//! talks to.
//!
//! The crate is the piece that knows the protocol's routes and the node's own
//! state. Sources, decoding, voice and lyrics are all reached through the
//! traits in [`playback`] and [`server::Dependencies`], so a node can be built
//! with the real implementations and exercised with stand-ins.

pub mod cpu;
pub mod playback;
pub mod player;
pub mod ratelimit;
pub mod server;
pub mod session;
pub mod webhook;

pub use playback::{Engine, Loader, Playback};
pub use player::{Deps, Player, PlayerError};
pub use server::{Dependencies, Server};
pub use session::Session;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// The node's own version, which the banner, `GET /api/v1/info` and the `Server`
/// header all report.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The independent Rustlink API major version.
pub const APIVERSION: &str = "1";

/// What a session sends every event to. A player holds one of these rather than
/// the session, so a destroyed player cannot keep a session alive.
pub type Emit = Arc<dyn Fn(rustlink_protocol::Object) + Send + Sync>;

/// The playback counters the node reports and the metrics expose.
#[derive(Default)]
pub struct Counters {
    /// Opus frames handed to Discord.
    pub sent: AtomicU64,
    /// Frame deadlines that were missed, counted in frames.
    pub deficit: AtomicU64,
    /// Tracks that ended in a failure.
    pub errors: AtomicU64,
}
