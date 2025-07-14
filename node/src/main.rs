use clap::Parser as _;
use seismicbft_node::args::CliArgs;

fn main() {
    let args = CliArgs::parse();
    args.exec()
}
