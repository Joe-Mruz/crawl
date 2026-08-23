//! WebTiles wire protocol: typed messages + the batching/compression codec.
//! See `../PROTOCOL.md` for the specification these types implement.

pub mod client;
pub mod codec;
pub mod server;

pub use client::{ClientMessage, ClientMessageParseError, KnownClientMessage};
pub use codec::{FrameCompressor, FrameDecompressor, MessageBatcher};
pub use server::{LobbyEntry, ServerMessage};
