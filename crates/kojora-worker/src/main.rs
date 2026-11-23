//! Long-lived, headless, single-host Kojora worker.
//! Private Unix socket only. This is not a network service or remote worker.

use kojora_runtime::ProcessLimits;
use kojora_studio::{
    checks::{CheckKind, CheckPolicy},
    default_data_dir,
    scheduler::AdmissionPolicy,
    worker::{serve_with_checks, WorkerClient},
    Studio,
};
use std::env;
use std::io;
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Serve,
    Status,
    Stop,
}

fn parse() -> io::Result<(PathBuf, ProcessLimits, Mode, AdmissionPolicy, CheckPolicy)> {
    let mut data_dir = default_data_dir()?;
    let mut limits = ProcessLimits::default();
    let mut admission = AdmissionPolicy::single();
    let mut checks = CheckPolicy::default();
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
            "--max-parallel" => {
                let value = args.next().ok_or_else(||
                    io::Error::new(io::ErrorKind::InvalidInput, "--max-parallel needs a number"))?;
                admission.max_parallel = value.to_string_lossy().parse().map_err(|_|
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid max-parallel"))?;
            }
            "--memory-reservation-mib" => {
                let value = args.next().ok_or_else(||
                    io::Error::new(io::ErrorKind::InvalidInput, "--memory-reservation-mib needs a number"))?;
                admission.memory_mib_per_task = value.to_string_lossy().parse().map_err(|_|
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid memory reservation"))?;
            }
            "--total-memory-reservation-mib" => {
                let value = args.next().ok_or_else(||
                    io::Error::new(io::ErrorKind::InvalidInput, "--total-memory-reservation-mib needs a number"))?;
                admission.total_memory_mib = value.to_string_lossy().parse().map_err(|_|
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid total memory reservation"))?;
            }
            "--enable-checks" => checks.enabled = true,
            "--post-edit-check" | "--pre-merge-check" => {
                let flag = argument.to_string_lossy();
                let value = args.next().ok_or_else(||
                    io::Error::new(io::ErrorKind::InvalidInput, "check flag requires a preset name"))?;
                let kind = match value.to_string_lossy().as_ref() {
                    "rustfmt" => CheckKind::RustFmt,
                    "clippy" => CheckKind::RustClippy,
                    "test" => CheckKind::RustTest,
                    "ruff" => CheckKind::Ruff,
                    "biome" => CheckKind::Biome,
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidInput,
                        "check must be rustfmt, clippy, test, ruff or biome")),
                };
                if flag == "--post-edit-check" {
                    if !checks.post_edit.contains(&kind) { checks.post_edit.push(kind); }
                } else if !checks.pre_merge.contains(&kind) {
                    checks.pre_merge.push(kind);
                }
            }
            "--check-timeout-seconds" => {
                let value = args.next().ok_or_else(||
                    io::Error::new(io::ErrorKind::InvalidInput, "missing check timeout"))?;
                checks.timeout_secs = value.to_string_lossy().parse().map_err(|_|
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid check timeout"))?;
            }
            "--max-pending-checks" => {
                let value = args.next().ok_or_else(||
                    io::Error::new(io::ErrorKind::InvalidInput, "missing check queue limit"))?;
                checks.max_pending = value.to_string_lossy().parse().map_err(|_|
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid check queue limit"))?;
            }
            "--status" if mode == Mode::Serve => mode = Mode::Status,
            "--stop" if mode == Mode::Serve => mode = Mode::Stop,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "usage: kojora-worker [--data-dir ABSOLUTE_DIR] [--address-space-mib N] [--cpu-seconds N] [--status|--stop]")),
        }
    }
    if !data_dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "data directory must be absolute",
        ));
    }
    limits.validate()?;
    admission.validate()?;
    if admission.max_parallel > 1 && limits.address_space_mib.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "parallel runs require an explicit per-process address-space ceiling",
        ));
    }
    checks.limits = limits;
    checks.validate()?;
    Ok((data_dir, limits, mode, admission, checks))
}

fn run() -> io::Result<()> {
    let (root, limits, mode, admission, checks) = parse()?;
    let socket = root.join("worker.sock");
    match mode {
        Mode::Serve => {
            let studio = Studio::open_with_limits_and_policy(root, limits, admission)?;
            serve_with_checks(studio, limits, checks)
        }
        Mode::Status => {
            let client = WorkerClient::connect(socket)?;
            let state = client.status()?;
            println!(
                "Worker pid={} protocol={} active_task={:?} active_count={} max_parallel={} reserved_mib={} checks_pending={} address_space_mib={:?} cpu_seconds={:?}",
                state.pid, state.protocol, state.active_task, state.active_count, state.concurrency,
                state.reserved_memory_mib, state.checks_pending, state.address_space_mib, state.cpu_seconds
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
