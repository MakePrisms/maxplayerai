mod cli;
mod relay;
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    std::process::exit(cli::run(
        &args,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ));
}
