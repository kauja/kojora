//! Long-lived, headless, single-host Kojora worker.
//! Private Unix socket only. This is not a network service or remote worker.

use kojora_runtime::ProcessLimits;
use kojora_studio::{default_data_dir, worker::{serve, WorkerClient}, Studio};
use std::env;
use std::io;
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Serve,
    Status,
    Stop,
}

fn parse() -> io::Result<(PathBuf, ProcessLimits, Mode)> {
    let mut data_dir = default_data_dir()?;
    let mut limits = ProcessLimits::default();
    let mut mode = Mode::Serve;
    let mut args = env::args_os().skip(1);
    while let Some(argument) = args.next() {
        match argument.to_string_lossy().as_ref() {
            "--data-dir" => {
                data_dir = PathBuf::from(args.next().ok_or_else(||
                    io::Error::new(io::ErrorKind::InvalidInput, "--data-dir requires a path"))?);
            }
            "--address-space-mib" => {
                let text = args.next().ok_or_else(||
                    io::Error::new(io::ErrorKind::InvalidInput, "--address-space-mib requires a number"))?;
                limits.address_space_mib = Some(text.to_string_lossy().parse().map_err(|_|
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid memory ceiling"))?);
            }
            "--cpu-seconds" => {
                let text = args.next().ok_or_else(||
                    io::Error::new(io::ErrorKind::InvalidInput, "--cpu-seconds requires a number"))?;
                limits.cpu_seconds = Some(text.to_string_lossy().parse().map_err(|_|
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid CPU-seconds ceiling"))?);
            }
            "--status" if mode == Mode::Serve => mode = Mode::Status,
            "--stop" if mode == Mode::Serve => mode = Mode::Stop,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "usage: kojora-worker [--data-dir ABSOLUTE_DIR] [--address-space-mib N] [--cpu-seconds N] [--status|--stop]")),
        }
    }
    if !data_dir.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "data directory must be absolute"));
    }
    limits.validate()?;
    Ok((data_dir, limits, mode))
}

fn run() -> io::Result<()> {
    let (root, limits, mode) = parse()?;
    let socket = root.join("worker.sock");
    match mode {
        Mode::Serve => {
            let studio = Studio::open_with_limits(root, limits)?;
            serve(studio, limits)
        }
        Mode::Status => {
            let client = WorkerClient::connect(socket)?;
            let state = client.status()?;
            println!(
                "Worker pid={} protocol={} active_task={:?} concurrency={} address_space_mib={:?} cpu_seconds={:?}",
                state.pid, state.protocol, state.active_task, state.concurrency,
                state.address_space_mib, state.cpu_seconds
            );
            Ok(())
        }
        Mode::Stop => {
            let client = WorkerClient::connect(socket)?;
            client.shutdown()?;
            println!("Worker shutdown requested");
            Ok(())
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("kojora-worker: {error}");
        std::process::exit(1);
    }
}
