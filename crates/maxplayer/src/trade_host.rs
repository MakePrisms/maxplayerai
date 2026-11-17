//! Trade's `serve` hosted inside the buyer daemon and the seller (`maxplayer_trade::host`), so a
//! home that trades is served without a separate `maxplayer trade serve`. `[trade] serve = false`
//! turns it off; `[trade] relays` / `mint_relays` pick its relays (the `maxplayer trade` defaults
//! read the same keys, so commands hand off to it).

use maxplayer_core::home::MaxplayerHome;
use maxplayer_trade::host::{self, Hosted};

/// Start hosting trade serve for `home` from inside the daemon's runtime, once the daemon owns its
/// home. `None` when `[trade] serve` is off or the `[trade]` relays are invalid (said once; the
/// daemon runs on without trading). TERM/INT for the hosted serve are listened for on the daemon's
/// own runtime, which lives as long as the process (review of #1334, M2).
pub fn start(home: &MaxplayerHome, who: &'static str) -> Option<Hosted> {
    if !home.config.trade.serve {
        return None;
    }
    match host::spawn(&home.root, options(home, who)) {
        Ok(hosted) => Some(hosted),
        Err(error) => {
            maxplayer_core::opline!("{who}: trade serve not hosted: {error:#}");
            None
        }
    }
}

fn options(home: &MaxplayerHome, who: &'static str) -> host::Options {
    let mut options = host::Options::new(
        who,
        home.config.trade.relays.clone(),
        home.config.trade.mint_relays.clone(),
    );
    options.signal_runtime = tokio::runtime::Handle::try_current().ok();
    options
}
