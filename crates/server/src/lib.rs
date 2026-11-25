//! Sessions, players, and the HTTP and WebSocket surface a client talks to.

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
use std::time::{SystemTime, UNIX_EPOCH};

/// The node's own version, reported in the banner, `GET /api/v1/info` and `Server`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The independent Rustlink API major version.
pub const APIVERSION: &str = "1";

/// Weakly-held session event callback so a player never retains its session.
pub type Emit = Arc<dyn Fn(rustlink_protocol::Object) + Send + Sync>;

/// Playback counters exposed via `/api/v1/stats`, `node.stats`, and `/metrics`.
#[derive(Default)]
pub struct Counters {
    pub sent: AtomicU64,
    pub deficit: AtomicU64,
    pub errors: AtomicU64,
}

pub(crate) fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as i64)
}
