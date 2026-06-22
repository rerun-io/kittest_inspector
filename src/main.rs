//! Binary entry point for the kittest inspector.
//!
//! The app itself lives in [`kittest_inspector`]; this file opens the transport, wires up a
//! cross-process single-instance guard, and runs the eframe event loop.
//!
//! The peer is reached over a cross-platform local socket (unix domain socket / Windows
//! named pipe) named by [`egui_inspection::INSPECTION_SOCKET_ENV_VAR`]. We pick our role by
//! probing it:
//!
//! - **connect**: if something is already listening, dial it. This is the kittest "spawn"
//!   path — `egui_kittest::InspectorPlugin` binds a socket, spawns this binary with the env
//!   var pointed at it, and accepts our connection.
//!
//! - **listen**: if nothing is listening yet, bind the name ourselves and accept one inbound
//!   connection. This is the standalone live-app path — launch this binary first, then start
//!   an `egui_inspection::InspectionPlugin` app with the same env var; the app dials in.
//!
//! Communication with the peer is bidirectional and asynchronous: a reader thread decodes
//! [`HarnessMessage`]s, a writer thread encodes [`InspectorCommand`]s, and the UI talks to
//! them only via mpsc channels. All logging goes to a file.

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::sync::mpsc;
use std::thread;

use eframe::egui;
use egui_inspection::transport::{self, RecvHalf, SendHalf};
use egui_inspection::{
    HarnessMessage, INSPECTION_SOCKET_ENV_VAR, InspectorCommand, read_message, write_message,
};
use kittest_inspector::{InspectorApp, log_diag};

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

    let (worker_tx, worker_rx) = mpsc::channel::<HarnessMessage>();
    let (command_tx, command_rx) = mpsc::channel::<InspectorCommand>();

    // Pick the transport once up front and spawn dedicated reader / writer threads. Doing
    // the accept *before* opening the window keeps the UI from flashing up if the user
    // misconfigured the env var.
    spawn_transport(worker_tx, command_rx);

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

/// Open the configured transport and spawn the reader + writer threads on it.
fn spawn_transport(
    worker_tx: mpsc::Sender<HarnessMessage>,
    command_rx: mpsc::Receiver<InspectorCommand>,
) {
    let Ok(socket) = std::env::var(INSPECTION_SOCKET_ENV_VAR) else {
        log_diag(&format!(
            "{INSPECTION_SOCKET_ENV_VAR} not set; opening with no peer connection"
        ));
        return;
    };
    match open_transport(&socket) {
        Ok((reader, writer)) => spawn_reader_writer(reader, writer, worker_tx, command_rx),
        Err(err) => log_diag(&format!("transport setup failed for {socket}: {err}")),
    }
}

/// Open the local socket named by `socket`, picking our role by probing it: dial it if a
/// listener is already waiting (kittest "spawn" path), otherwise bind it ourselves and accept
/// one inbound connection (standalone live-app path).
fn open_transport(
    socket: &str,
) -> io::Result<(BufReader<RecvHalf>, BufWriter<SendHalf>)> {
    match transport::connect(socket) {
        Ok((reader, writer)) => {
            log_diag(&format!("connected to {socket}"));
            Ok((BufReader::new(reader), BufWriter::new(writer)))
        }
        Err(connect_err) => {
            // Nothing listening yet — become the listener instead.
            log_diag(&format!(
                "connect to {socket} failed ({connect_err}); binding as listener"
            ));
            // On unix the name is a filesystem path; clear any stale socket file from a
            // crashed previous run, or `bind` fails with `EADDRINUSE`. Harmless elsewhere.
            #[cfg(unix)]
            let _ = std::fs::remove_file(socket);
            let listener = transport::Listener::bind(socket)?;
            log_diag(&format!("listening on {socket}"));
            let (reader, writer) = listener.accept()?;
            log_diag(&format!("accepted connection on {socket}"));
            Ok((BufReader::new(reader), BufWriter::new(writer)))
        }
    }
}

fn spawn_reader_writer<R, W>(
    reader: R,
    writer: W,
    worker_tx: mpsc::Sender<HarnessMessage>,
    command_rx: mpsc::Receiver<InspectorCommand>,
) where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    thread::Builder::new()
        .name("kittest_inspector_read".into())
        .spawn(move || run_reader(reader, &worker_tx))
        .expect("spawn reader thread");

    thread::Builder::new()
        .name("kittest_inspector_write".into())
        .spawn(move || run_writer(writer, &command_rx))
        .expect("spawn writer thread");
}

/// Decode harness messages until EOF and forward them to the UI. Returning drops the
/// sender, which the UI sees as `TryRecvError::Disconnected`.
fn run_reader<R: BufRead>(mut reader: R, ui_tx: &mpsc::Sender<HarnessMessage>) {
    loop {
        match read_message::<_, HarnessMessage>(&mut reader) {
            Ok(msg) => {
                if ui_tx.send(msg).is_err() {
                    return;
                }
            }
            Err(err) => {
                if err.kind() != io::ErrorKind::UnexpectedEof {
                    log_diag(&format!("read failed: {err}"));
                }
                return;
            }
        }
    }
}

/// Drain the UI's outgoing command queue and write each command to the peer.
fn run_writer<W: Write>(mut writer: W, command_rx: &mpsc::Receiver<InspectorCommand>) {
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
