mod accept_cli;
mod buyer;
mod cli;
mod collect_cli;
#[cfg(feature = "wallet")]
mod daemon;
mod doctor;
mod exec;
mod mcp;
mod profile_cli;
// The `sell` surface is the seller advertise path: it publishes the kind-0 identity and boots the
// heartbeat loop that announces capability. `acp` compiles in the agent-execution that lets a seat
// actually deliver;
// without it a booted seller advertises a seat it can never run a job on and a buyer loses the sats
// at award (#360). Gate the whole surface on `acp` so a buyer-only build cannot advertise at all.
#[cfg(feature = "acp")]
mod sell;
// The containment probe is compiled on every build, not only the seller one: the payload half runs
// INSIDE the launcher, and a seat may reasonably run the probe from a binary it already trusts.
#[cfg(feature = "wallet")]
mod sandbox_probe;
#[cfg(feature = "stub-pay")]
mod stub_pay_cli;
mod wallet_cli;
mod whoami;

fn main() {
    let code = cli::run(
        std::env::args(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}
