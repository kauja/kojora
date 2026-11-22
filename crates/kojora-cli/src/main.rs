use kojora_runtime::{Executor, LocalExecutor, RunSpec};
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

fn print_help() {
    println!("Kojora — a modular, resource-aware software factory");
    println!();
    println!("Usage: kojora <COMMAND>");
    println!();
    println!("Commands:");
    println!("  run [--cwd DIR] [--log-root DIR] -- PROGRAM [ARG ...]");
    println!("               Run one trusted command and save output and events");
    println!("  doctor       Show current implementation status");
    println!("  version      Print the version");
    println!("  help         Show this help message");
    println!();
    println!("Kojora is pre-alpha. 'run' is not sandboxed or a daemon.");
}

fn run(args: &[OsString]) -> Result<ExitCode, String> {
    let mut cwd = env::current_dir().map_err(|error| error.to_string())?;
    let mut root = None;
    let mut index = 0;
    loop {
        match args.get(index).map(OsString::as_os_str) {
            Some(value) if value == "--cwd" => {
                let path = args.get(index + 1).ok_or("--cwd requires a directory")?;
                cwd = PathBuf::from(path);
                index += 2;
            }
            Some(value) if value == "--log-root" => {
                let path = args.get(index + 1).ok_or("--log-root requires a directory")?;
                root = Some(PathBuf::from(path));
                index += 2;
            }
            Some(value) if value == "--" => {
                index += 1;
                break;
            }
            Some(value) => {
                return Err(format!("unknown run option: {}", value.to_string_lossy()));
            }
            None => return Err("expected -- PROGRAM [ARG ...]".to_owned()),
        }
    }
    let program = args.get(index).ok_or("expected a program after --")?;
    let log_root = root.unwrap_or_else(|| cwd.join(".kojora").join("runs"));
    let spec = RunSpec {
        program: program.clone(),
        args: args[index + 1..].to_vec(),
        cwd,
        log_root,
    };
    let outcome = LocalExecutor
        .run(&spec)
        .map_err(|error| format!("could not execute command: {error}"))?;
    eprintln!("run directory: {}", outcome.run_dir.display());
    eprintln!(
        "completed: success={}, exit_code={:?}, duration_ms={}, stdout_bytes={}, stderr_bytes={}",
        outcome.success,
        outcome.exit_code,
        outcome.duration.as_millis(),
        outcome.stdout_bytes,
        outcome.stderr_bytes
    );
    Ok(match outcome.exit_code {
        Some(code) if (0..=255).contains(&code) => ExitCode::from(code as u8),
        _ => ExitCode::FAILURE,
    })
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    match args.first().map(OsString::as_os_str) {
        None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(value) if value == "help" || value == "--help" || value == "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(value) if value == "version" || value == "--version" || value == "-V" => {
            println!("kojora {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(value) if value == "doctor" => {
            println!("Kojora process runner: available (local, synchronous, no sandbox)");
            println!("Agent adapters: not implemented");
            println!("Headless daemon / API: not implemented");
            println!("GUI: not implemented");
            ExitCode::SUCCESS
        }
        Some(value) if value == "run" => match run(&args[1..]) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        },
        Some(other) => {
            eprintln!("Unknown command: {}", other.to_string_lossy());
            eprintln!("Try: kojora --help");
            ExitCode::FAILURE
        }
    }
}
