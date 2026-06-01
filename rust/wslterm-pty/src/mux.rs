//! Multiplexes many PTY sessions over one wslg.exe + one wslptyd process.
//! Ports `src/WslTerminal/WslMux.cs`. A background reader thread demuxes server
//! frames and delivers per-session events to the owner over an mpsc channel;
//! writes are serialized under a mutex.

use std::collections::HashSet;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::process::WslProcess;
use crate::protocol::{self, T_CLOSE, T_DATA, T_EXIT, T_OPEN, T_RESIZE, T_SIGNAL};

/// Bound on queued, not-yet-consumed server frames. When the owner falls behind
/// (e.g. a flood like termbench), the reader blocks here, which fills the OS
/// pipe and back-pressures wslptyd — keeping memory bounded instead of buffering
/// the entire burst. Frames are at most a 64KB PTY chunk, so worst case is tens
/// of MB, typically far less.
const MUX_CHANNEL_BOUND: usize = 512;

/// An event from the server for a session, delivered on the mux channel.
#[derive(Debug)]
pub enum MuxEvent {
    /// Raw PTY output for `id`.
    Data { id: u32, bytes: Vec<u8> },
    /// Session `id` ended with `code` (or -1 if the whole server died).
    Exit { id: u32, code: i32 },
}

struct Shared {
    stdin: Mutex<ChildStdin>,
    dead: AtomicBool,
    live: Mutex<HashSet<u32>>,
}

impl Shared {
    fn write_frame(&self, id: u32, ty: u8, payload: &[u8]) {
        if self.dead.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self.stdin.lock().unwrap();
        if protocol::write_frame(&mut *guard, id, ty, payload).is_err() {
            self.dead.store(true, Ordering::Release);
        }
    }
}

pub struct WslMux {
    shared: Arc<Shared>,
    next_id: AtomicU32,
    reader: Option<JoinHandle<()>>,
    proc: Arc<Mutex<WslProcess>>,
}

impl WslMux {
    /// Start a mux over a launched server process. Returns the mux and the
    /// receiver the owner polls for `MuxEvent`s.
    pub fn start(proc: WslProcess) -> (WslMux, Receiver<MuxEvent>) {
        let mut proc = proc;
        let (stdin, stdout) = proc.take_stdio();
        let shared = Arc::new(Shared {
            stdin: Mutex::new(stdin),
            dead: AtomicBool::new(false),
            live: Mutex::new(HashSet::new()),
        });
        let (tx, rx) = std::sync::mpsc::sync_channel(MUX_CHANNEL_BOUND);
        let reader_shared = shared.clone();
        let reader = std::thread::Builder::new()
            .name("wsl-mux-reader".into())
            .spawn(move || reader_loop(stdout, reader_shared, tx))
            .expect("spawn reader");
        (
            WslMux {
                shared,
                next_id: AtomicU32::new(0),
                reader: Some(reader),
                proc: Arc::new(Mutex::new(proc)),
            },
            rx,
        )
    }

    pub fn is_dead(&self) -> bool {
        self.shared.dead.load(Ordering::Acquire)
    }

    /// Open a new PTY session; returns its id. Ports `Open`.
    pub fn open(&self, cols: u16, rows: u16, cwd: &str) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared.live.lock().unwrap().insert(id);
        self.shared.write_frame(id, T_OPEN, &protocol::open_payload(cols, rows, cwd));
        id
    }

    pub fn send_data(&self, id: u32, data: &[u8]) {
        self.shared.write_frame(id, T_DATA, data);
    }
    pub fn send_resize(&self, id: u32, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.shared.write_frame(id, T_RESIZE, &protocol::resize_payload(cols, rows));
    }
    pub fn send_signal(&self, id: u32, signo: u8) {
        self.shared.write_frame(id, T_SIGNAL, &[signo]);
    }
    pub fn close(&self, id: u32) {
        self.shared.write_frame(id, T_CLOSE, &[]);
        self.shared.live.lock().unwrap().remove(&id);
    }
}

impl Drop for WslMux {
    fn drop(&mut self) {
        // Closing stdin makes the server EOF and exit, unblocking the reader.
        self.shared.dead.store(true, Ordering::Release);
        if let Ok(mut p) = self.proc.lock() {
            p.kill();
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

fn reader_loop<R: std::io::Read>(mut stdout: R, shared: Arc<Shared>, tx: SyncSender<MuxEvent>) {
    let mut scratch = vec![0u8; 65536];
    loop {
        match protocol::read_frame(&mut stdout, &mut scratch) {
            Ok(Some((id, ty, len))) => match ty {
                T_DATA => {
                    if tx.send(MuxEvent::Data { id, bytes: scratch[..len].to_vec() }).is_err() {
                        break;
                    }
                }
                T_EXIT => {
                    let code = if len >= 4 {
                        i32::from_le_bytes(scratch[..4].try_into().unwrap())
                    } else {
                        0
                    };
                    shared.live.lock().unwrap().remove(&id);
                    let _ = tx.send(MuxEvent::Exit { id, code });
                }
                _ => {}
            },
            Ok(None) => break, // EOF
            Err(_) => break,
        }
    }
    shared.dead.store(true, Ordering::Release);
    // Tell the owner every still-live session ended.
    let live: Vec<u32> = shared.live.lock().unwrap().drain().collect();
    for id in live {
        let _ = tx.send(MuxEvent::Exit { id, code: -1 });
    }
}

