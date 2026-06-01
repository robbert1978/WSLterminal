//! The wslptyd multiplex wire protocol, independent of the transport.
//! Ports the framing in `src/WslTerminal/WslMux.cs`. Length-prefixed frames:
//!   `[u32 session][u8 type][u32 len][payload]`  (all little-endian)
//!
//! Host -> server: 1 OPEN, 2 DATA, 3 RESIZE, 4 SIGNAL, 5 CLOSE
//! Server -> host: 2 DATA, 6 EXIT
//!
//! Kept transport-free so it unit-tests over in-memory buffers; `mux.rs` wires
//! it to the real wslg.exe pipes on Windows.

use std::io::{self, Read, Write};

pub const T_OPEN: u8 = 1;
pub const T_DATA: u8 = 2;
pub const T_RESIZE: u8 = 3;
pub const T_SIGNAL: u8 = 4;
pub const T_CLOSE: u8 = 5;
pub const T_EXIT: u8 = 6;

/// Sanity bound matching the C# reader (drop the connection past this).
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// Write one frame to `w`. Header + payload in two writes, then flush — matching
/// the C# `WriteFrame`.
pub fn write_frame<W: Write>(w: &mut W, id: u32, ty: u8, payload: &[u8]) -> io::Result<()> {
    let mut hdr = [0u8; 9];
    hdr[0..4].copy_from_slice(&id.to_le_bytes());
    hdr[4] = ty;
    hdr[5..9].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&hdr)?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    w.flush()
}

/// Build an OPEN payload: `[u16 cols][u16 rows][u32 cwdLen][cwd][u32 shellLen=0]`.
/// Ports the body of `WslMux.Open`.
pub fn open_payload(cols: u16, rows: u16, cwd: &str) -> Vec<u8> {
    let cwd_b = cwd.as_bytes();
    let mut p = Vec::with_capacity(2 + 2 + 4 + cwd_b.len() + 4);
    p.extend_from_slice(&cols.max(1).to_le_bytes());
    p.extend_from_slice(&rows.max(1).to_le_bytes());
    p.extend_from_slice(&(cwd_b.len() as u32).to_le_bytes());
    p.extend_from_slice(cwd_b);
    p.extend_from_slice(&0u32.to_le_bytes()); // shell len 0 => $SHELL
    p
}

pub fn resize_payload(cols: u16, rows: u16) -> [u8; 4] {
    let mut p = [0u8; 4];
    p[0..2].copy_from_slice(&cols.to_le_bytes());
    p[2..4].copy_from_slice(&rows.to_le_bytes());
    p
}

/// A frame read from the server.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub id: u32,
    pub ty: u8,
    pub payload: Vec<u8>,
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => return Ok(false), // EOF
            Ok(k) => got += k,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Read one frame from `r`, reusing `scratch` as the growable payload buffer (it
/// is *not* reallocated per frame — mirrors the v1.0.3 reader-buffer fix). On a
/// DATA/EXIT etc. the payload slice is `scratch[..len]`. Returns `Ok(None)` at EOF.
pub fn read_frame<R: Read>(r: &mut R, scratch: &mut Vec<u8>) -> io::Result<Option<(u32, u8, usize)>> {
    let mut hdr = [0u8; 9];
    if !read_full(r, &mut hdr)? {
        return Ok(None);
    }
    let id = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
    let ty = hdr[4];
    let len = u32::from_le_bytes(hdr[5..9].try_into().unwrap());
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let len = len as usize;
    if len > scratch.len() {
        scratch.resize(len, 0);
    }
    if len > 0 && !read_full(r, &mut scratch[..len])? {
        return Ok(None);
    }
    Ok(Some((id, ty, len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_over_buffer() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 7, T_DATA, b"hello").unwrap();
        write_frame(&mut buf, 7, T_EXIT, &0u32.to_le_bytes()).unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let mut scratch = vec![0u8; 8];

        let (id, ty, len) = read_frame(&mut cur, &mut scratch).unwrap().unwrap();
        assert_eq!((id, ty), (7, T_DATA));
        assert_eq!(&scratch[..len], b"hello");

        let (id, ty, len) = read_frame(&mut cur, &mut scratch).unwrap().unwrap();
        assert_eq!((id, ty), (7, T_EXIT));
        assert_eq!(u32::from_le_bytes(scratch[..len].try_into().unwrap()), 0);

        assert!(read_frame(&mut cur, &mut scratch).unwrap().is_none()); // EOF
    }

    #[test]
    fn open_payload_layout() {
        let p = open_payload(80, 24, "/home/u");
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), 80);
        assert_eq!(u16::from_le_bytes([p[2], p[3]]), 24);
        assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 7); // "/home/u".len()
        assert_eq!(&p[8..15], b"/home/u");
        assert_eq!(u32::from_le_bytes(p[15..19].try_into().unwrap()), 0); // shell len
    }

    #[test]
    fn open_payload_clamps_zero_dims() {
        let p = open_payload(0, 0, "");
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), 1);
        assert_eq!(u16::from_le_bytes([p[2], p[3]]), 1);
    }

    #[test]
    fn read_frame_reuses_scratch_buffer() {
        // A large frame grows scratch; a later small frame must not shrink it
        // (the reuse that fixed the LOH churn in v1.0.3).
        let mut buf = Vec::new();
        write_frame(&mut buf, 1, T_DATA, &vec![0xAB; 100_000]).unwrap();
        write_frame(&mut buf, 1, T_DATA, b"x").unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let mut scratch = vec![0u8; 8];

        let (_, _, len) = read_frame(&mut cur, &mut scratch).unwrap().unwrap();
        assert_eq!(len, 100_000);
        let cap_after_big = scratch.len();

        let (_, _, len) = read_frame(&mut cur, &mut scratch).unwrap().unwrap();
        assert_eq!(len, 1);
        assert_eq!(&scratch[..1], b"x");
        assert!(scratch.len() >= cap_after_big); // not reallocated smaller
    }
}
