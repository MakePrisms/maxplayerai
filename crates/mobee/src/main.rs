mod agent_presets;
mod cli;
mod doctor;
mod exec;
mod mcp;
mod sell;
mod wallet_cli;

fn main() {
    let code = cli::run(
        std::env::args(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}
