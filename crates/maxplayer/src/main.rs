mod accept_cli;
// #818: the build-time commit stamp `--version` prints. Compiled on every build — provenance is not
// a feature, and a deployed binary that cannot say what it was built from is the state #818 measured.
mod build_stamp;
mod buyer;
mod cli;
mod collect_cli;
// Container-side delivery orchestrator entrypoint (`maxplayer __deliver …`, Track B). INTERNAL.
mod deliver_cli;
#[cfg(feature = "wallet")]
mod daemon;
mod doctor;
mod exec;
mod issuer_cli;
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
// The retired-seat holder reaper (#905). `acp`, not `wallet`: it calls `sandbox_netns::reap_orphans`,
// which needs the docker runner behind that feature â the same gate the boot reaper carries.
#[cfg(feature = "acp")]
mod sandbox_reap;
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
