//! Multiplexes many PTY sessions over one wslg.exe + one wslptyd process.
//! Ports `src/WslTerminal/WslMux.cs`. A background reader thread demuxes server
//! frames and delivers per-session events to the owner over an mpsc channel;
//! writes are serialized under a mutex.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::process::WslProcess;
use crate::protocol::{self, T_CLOSE, T_DATA, T_EXIT, T_INFO, T_OPEN, T_RESIZE, T_SIGNAL};

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
    /// The daemon's self-detected WSL distro registration name (sent once on connect).
    Info { distro: String },
}

struct Shared {
    stdin: Mutex<Box<dyn Write + Send>>,
    dead: AtomicBool,
    live: Mutex<HashSet<u32>>,
}

impl Shared {
    fn write_frame(&self, id: u32, ty: u8, payload: &[u8]) {
        if self.dead.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self.stdin.lock().unwrap();
        if protocol::write_frame(&mut **guard, id, ty, payload).is_err() {
            self.dead.store(true, Ordering::Release);
        }
    }
}

pub struct WslMux {
    shared: Arc<Shared>,
    next_id: AtomicU32,
    reader: Option<JoinHandle<()>>,
    /// Called once on drop to tear the transport down: kill the wslg.exe child
    /// (pipe mode) or shut down the vsock socket (which unblocks the reader).
    teardown: Option<Box<dyn FnMut() + Send>>,
}

impl WslMux {
    /// Start a mux over any transport: a reader, a writer, and a `teardown`
    /// closure run once on drop. The frame protocol is transport-agnostic, so the
    /// same mux drives the wslg pipe or a vsock socket.
    pub fn start(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        teardown: Box<dyn FnMut() + Send>,
    ) -> (WslMux, Receiver<MuxEvent>) {
        let shared = Arc::new(Shared {
            stdin: Mutex::new(writer),
            dead: AtomicBool::new(false),
            live: Mutex::new(HashSet::new()),
        });
        let (tx, rx) = std::sync::mpsc::sync_channel(MUX_CHANNEL_BOUND);
        let reader_shared = shared.clone();
        let handle = std::thread::Builder::new()
            .name("wsl-mux-reader".into())
            .spawn(move || reader_loop(reader, reader_shared, tx))
            .expect("spawn reader");
        (
            WslMux {
                shared,
                next_id: AtomicU32::new(0),
                reader: Some(handle),
                teardown: Some(teardown),
            },
            rx,
        )
    }

    /// Mux over a launched `wslg.exe` process (the pipe transport). Dropping the
    /// mux kills the child.
    pub fn from_process(mut proc: WslProcess) -> (WslMux, Receiver<MuxEvent>) {
        let (stdin, stdout) = proc.take_stdio();
        let proc = Arc::new(Mutex::new(proc));
        let p2 = proc.clone();
        Self::start(
            Box::new(stdout),
            Box::new(stdin),
            Box::new(move || {
                if let Ok(mut p) = p2.lock() {
                    p.kill();
                }
            }),
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
        // Tear down the transport (kill the child / shutdown the socket); that
        // unblocks the reader thread's read so the join below returns.
        self.shared.dead.store(true, Ordering::Release);
        if let Some(mut teardown) = self.teardown.take() {
            teardown();
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

fn reader_loop(mut stdout: Box<dyn Read + Send>, shared: Arc<Shared>, tx: SyncSender<MuxEvent>) {
    let mut scratch = vec![0u8; 65536];
    loop {
        match protocol::read_frame(&mut *stdout, &mut scratch) {
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
                T_INFO => {
                    if let Ok(s) = std::str::from_utf8(&scratch[..len]) {
                        if !s.is_empty() && tx.send(MuxEvent::Info { distro: s.to_string() }).is_err() {
                            break;
                        }
                    }
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

