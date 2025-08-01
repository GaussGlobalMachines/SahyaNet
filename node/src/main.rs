#![feature(unsigned_abs)]
use clap::Parser as _;
use summit::args::CliArgs;

fn main() {
    let args = CliArgs::parse();
    args.exec()
}
