//! `maxplayer-mint`: an opt-in Cashu mint for seller credits, reachable only over Nostr relays
//! (docs/specs/seller-credits.md §3–§4). The wallet side is `maxplayer_core::nostr_mint`; the
//! envelope both sides speak is `maxplayer_core::mint_wire`.
//!
//! - [`home`]: `<home>/mint/` (Nostr key, seed, `mint.sqlite`, `mint.toml`).
//! - [`backend`]: the cdk mint behind the transport.
//! - [`dispatch`]: one envelope `op` → one cdk mint call.
//! - [`issue`]: local issue into a token file.
//! - [`replay`]: the durable request log (replay and reconcile).
//! - [`server`]: the relay listener (kind 23410 in, 23411 out).

pub mod backend;
pub mod dispatch;
pub mod home;
pub mod issue;
pub mod replay;
pub mod server;
