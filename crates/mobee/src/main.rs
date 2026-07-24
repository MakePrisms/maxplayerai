mod accept_cli;
mod agent_presets;
mod buyer;
mod cli;
mod collect_cli;
#[cfg(feature = "wallet")]
mod daemon;
mod doctor;
mod exec;
mod mcp;
mod profile_cli;
mod sell;
#[cfg(feature = "stub-pay")]
mod stub_pay_cli;
mod wallet_cli;

fn main() {
    let code = cli::run(
        std::env::args(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}
