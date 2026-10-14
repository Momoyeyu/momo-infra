use clap::Parser;
use momo_infra::cli::Cli;

fn main() {
    let cli = Cli::parse();
    std::process::exit(momo_infra::run(cli));
}
