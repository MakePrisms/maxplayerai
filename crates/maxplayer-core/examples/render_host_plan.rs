//! Prints the **host-side** iptables plan [`HostPolicy`] would hand its applier, for gate evidence.
//!
//! Sibling of `render_net_plan`, and it exists for the same reason: a gate script must install the
//! rules the PRODUCT renders, never rules a script author transcribed. A transcription drifts the
//! moment the policy changes, and the gate then keeps passing against a firewall the product no
//! longer builds — the exact failure these gates exist to catch.
//!
//! It matters more here than for the namespace plan. The host plan lands in chains that are shared
//! with every other container on the daemon, so a hand-written approximation of it in a script is
//! not just drift, it is a rule keyed to the wrong source touching someone else's traffic.
//!
//! ```text
//! cargo run -p maxplayer-core --example render_host_plan -- 172.18.0.2
//! cargo run -p maxplayer-core --example render_host_plan -- 172.18.0.2 --teardown
//! ```
//!
//! Arguments: the job namespace's address, then optionally `--teardown` for the exact inverse plan.

use maxplayer_core::sandbox_net::HostPolicy;
use maxplayer_core::sandbox_netns::{host_install_stdin, host_teardown_stdin};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(job_addr) = args.next() else {
        eprintln!("usage: render_host_plan <job_addr> [--teardown]");
        std::process::exit(2);
    };
    let teardown = match args.next().as_deref() {
        None => false,
        Some("--teardown") => true,
        Some(other) => {
            eprintln!("unknown argument {other:?} — expected --teardown or nothing");
            std::process::exit(2);
        }
    };

    let policy = HostPolicy { job_addr };
    let (plan, count) =
        if teardown { host_teardown_stdin(&policy) } else { host_install_stdin(&policy) };
    eprintln!("# {count} rules ({})", if teardown { "teardown" } else { "install" });
    print!("{plan}");
}
