//! Binary entry point for the kittest inspector.
//!
//! The app itself lives in [`kittest_inspector`]; this file only wires up stdin/stdout I/O,
//! a cross-process single-instance guard, and the eframe event loop.
//!
//! Communication with the harness is bidirectional and asynchronous: we read
//! [`HarnessMessage`]s from stdin on a background thread and write [`InspectorCommand`]s to
//! stdout on another, so the UI never has to coordinate with the harness on a request/reply
//! basis. All logging goes to a file.

use std::io::{self, BufReader, BufWriter};
use std::sync::mpsc;
use std::thread;

use eframe::egui;
use egui_kittest::inspector_api::{HarnessMessage, InspectorCommand, read_message, write_message};
use kittest_inspector::{CommandRx, InspectorApp, WorkerEvent, log_diag};

fn main() -> eframe::Result<()> {
    // Install a panic hook that writes to our own log file (not the inherited — and
    // potentially captured — stderr of the harness). Whatever the main thread or a spawned
    // worker does, we always get a breadcrumb on disk.
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_diag(&format!("PANIC: {info}"));
        default(info);
    }));

    // Cross-process single-instance guard. If another inspector is already running, block
    // here until that window closes. Held for the lifetime of `_lock`; the OS releases the
    // flock when the file descriptor is dropped on exit.
    let _lock = acquire_single_instance_lock();

    let (worker_tx, worker_rx) = mpsc::channel::<WorkerEvent>();
    let (command_tx, command_rx) = mpsc::channel::<InspectorCommand>();

    thread::Builder::new()
        .name("kittest_inspector_read".into())
        .spawn(move || run_reader(&worker_tx))
        .expect("spawn reader thread");

    thread::Builder::new()
        .name("kittest_inspector_write".into())
        .spawn(move || run_writer(&command_rx))
        .expect("spawn writer thread");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("kittest inspector")
            .with_inner_size([1100.0, 750.0]),
        ..Default::default()
    };

    eframe::run_native(
        "kittest inspector",
        options,
        Box::new(|cc| Ok(Box::new(InspectorApp::new(cc, worker_rx, command_tx)))),
    )
}

/// Read frames from stdin and forward to the UI until the harness disconnects.
fn run_reader(ui_tx: &mpsc::Sender<WorkerEvent>) {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());

    loop {
        match read_message::<_, HarnessMessage>(&mut reader) {
            Ok(HarnessMessage::Frame(frame)) => {
                if ui_tx.send(WorkerEvent::Frame(frame)).is_err() {
                    return;
                }
            }
            Ok(HarnessMessage::Goodbye) => {
                let _ = ui_tx.send(WorkerEvent::Disconnected);
                return;
            }
            Err(err) => {
                if err.kind() != io::ErrorKind::UnexpectedEof {
                    log_diag(&format!("read failed: {err}"));
                }
                let _ = ui_tx.send(WorkerEvent::Disconnected);
                return;
            }
        }
    }
}

/// Drain the UI's outgoing command queue and write each command to the harness via stdout.
fn run_writer(command_rx: &CommandRx) {
    let stdout = io::stdout();
    let mut writer = BufWriter::new(stdout.lock());

    while let Ok(cmd) = command_rx.recv() {
        if let Err(err) = write_message(&mut writer, &cmd) {
            log_diag(&format!("write failed: {err}"));
            return;
        }
    }
}

/// Try to acquire a cross-process exclusive lock on a well-known file so that only one
/// inspector window can be open on the machine at a time. Blocks here (before we open any
/// windows or touch stdio beyond this stderr line) if another inspector is already running.
fn acquire_single_instance_lock() -> Option<std::fs::File> {
    use fs4::fs_std::FileExt;

    // We specifically need a stable, cross-process path here — tempfile's per-process dir
    // can't serve as a system-wide mutex.
    #[expect(clippy::disallowed_methods)]
    let path = std::env::temp_dir().join("kittest_inspector.lock");

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(err) => {
            log_diag(&format!(
                "couldn't open lock file {}: {err} (running without single-instance guard)",
                path.display()
            ));
            return None;
        }
    };

    match FileExt::lock_exclusive(&file) {
        Ok(()) => Some(file),
        Err(err) => {
            log_diag(&format!("failed to acquire lock: {err}"));
            None
        }
    }
}
