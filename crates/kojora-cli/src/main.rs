use std::env;
use std::process::ExitCode;

fn print_help() {
    println!("Kojora — a modular, resource-aware software factory");
    println!();
    println!("Usage: kojora <COMMAND>");
    println!();
    println!("Commands:");
    println!("  doctor       Show current implementation status");
    println!("  version      Print the version");
    println!("  help         Show this help message");
    println!();
    println!("Status: pre-alpha scaffold; agent execution is not yet implemented.");
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("version") | Some("--version") | Some("-V") => {
            println!("kojora {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("doctor") => {
            println!("Kojora core: scaffold available");
            println!("Agent runtime: not implemented");
            println!("Headless service: not implemented");
            println!("GUI: not implemented");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("Unknown command: {other}");
            eprintln!("Try: kojora --help");
            ExitCode::FAILURE
        }
    }
}
