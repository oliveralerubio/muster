#![forbid(unsafe_code)]

fn main() {
    // SIGINT ends the process with the documented "interrupted" code. If the
    // handler cannot be installed the default signal behaviour still applies.
    let _ = ctrlc::set_handler(|| std::process::exit(130));
    std::process::exit(muster::cli::main(std::env::args_os()));
}
