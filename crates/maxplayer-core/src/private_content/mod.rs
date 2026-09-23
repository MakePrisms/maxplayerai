//! Private job transport and persistence. The relay shares the exact closed schema
//! through maxplayer-private-protocol, without linking the marketplace/wallet runtime.
pub use maxplayer_private_protocol::*;
pub mod builders;
#[cfg(feature = "wallet")]
pub mod hosting;
#[cfg(feature = "wallet")]
pub mod inputs;
#[cfg(feature = "wallet")]
pub mod invoice;
#[cfg(feature = "wallet")]
pub mod session;
pub mod settlement;
#[cfg(feature = "wallet")]
pub mod store;
#[cfg(test)]
mod tests;
pub mod transport;
pub mod wire;
