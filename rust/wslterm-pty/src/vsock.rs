//! vsock (AF_HYPERV) transport: connect the Windows host straight to `wslptyd`
//! listening on AF_VSOCK inside the WSL2 utility VM — no `wslg.exe` pipe.
//!
//! Mirrors the standalone `wsl_vsock_proxy.cc`: find the VM's GUID from the HCS
//! volatile registry (`ComputeSystemType == 2`), then `connect` an `AF_HYPERV`
//! `SOCK_STREAM`/`HV_PROTOCOL_RAW` socket to the Linux-vsock service GUID
//! `{port}-facb-11e6-bd58-64006a7986d3`. The accepted socket carries the same
//! length-prefixed mux protocol the wslg pipe did, so `WslMux` is unchanged.

#![cfg(windows)]

use std::io::{self, Read, Write};
use std::sync::{Arc, Once};

use windows_sys::Win32::Networking::WinSock::{
    closesocket, connect as ws_connect, getsockopt, ioctlsocket, recv, select, send, shutdown,
    socket, WSAGetLastError, WSAStartup, FD_SET, INVALID_SOCKET, SD_BOTH, SOCKADDR, SOCKET,
    SOCKET_ERROR, SOCK_STREAM, SOL_SOCKET, SO_ERROR, TIMEVAL, WSADATA, WSAEWOULDBLOCK,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    REG_DWORD,
};

use crate::mux::{MuxEvent, WslMux};

const AF_HYPERV: u16 = 34;
const HV_PROTOCOL_RAW: i32 = 1;
const FIONBIO: i32 = 0x8004667Eu32 as i32;
const CONNECT_TIMEOUT_MS: i32 = 500;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[repr(C)]
struct SockaddrHv {
    family: u16,
    reserved: u16,
    vm_id: Guid,
    service_id: Guid,
}

/// vsock service GUID for `port`: Data1 = port, rest is the fixed Linux template.
fn vsock_service_guid(port: u32) -> Guid {
    Guid { data1: port, data2: 0xfacb, data3: 0x11e6, data4: [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3] }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ensure_wsa() {
    static WSA: Once = Once::new();
    WSA.call_once(|| unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        WSAStartup(0x0202, &mut data);
    });
}

fn last_err() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// Parse a bare GUID string (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`).
fn parse_guid(s: &str) -> Option<Guid> {
    let s = s.trim().trim_start_matches('{').trim_end_matches('}');
    let p: Vec<&str> = s.split('-').collect();
    if p.len() != 5 || p[3].len() != 4 || p[4].len() != 12 {
        return None;
    }
    let d1 = u32::from_str_radix(p[0], 16).ok()?;
    let d2 = u16::from_str_radix(p[1], 16).ok()?;
    let d3 = u16::from_str_radix(p[2], 16).ok()?;
    let d4a = u16::from_str_radix(p[3], 16).ok()?;
    let d4b = u64::from_str_radix(p[4], 16).ok()?;
    let mut data4 = [0u8; 8];
    data4[0] = (d4a >> 8) as u8;
    data4[1] = d4a as u8;
    data4[2..8].copy_from_slice(&d4b.to_be_bytes()[2..8]);
    Some(Guid { data1: d1, data2: d2, data3: d3, data4 })
}

/// The running WSL utility VM's GUID, from the HCS volatile store
/// (`ComputeSystemType == 2`). `None` if no VM is running. Ports `get_wsl_vmid`.
fn find_wsl_vmid() -> Option<Guid> {
    const PATH: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\HostComputeService\VolatileStore\ComputeSystem";
    unsafe {
        let path = wide(PATH);
        let mut hkey: HKEY = 0;
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return None;
        }
        let mut found = None;
        let mut i = 0u32;
        loop {
            let mut name = [0u16; 128];
            let mut name_len = name.len() as u32;
            let rc = RegEnumKeyExW(
                hkey,
                i,
                name.as_mut_ptr(),
                &mut name_len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            i += 1;
            if rc != 0 {
                break; // ERROR_NO_MORE_ITEMS
            }
            let mut hsub: HKEY = 0;
            if RegOpenKeyExW(hkey, name.as_ptr(), 0, KEY_READ, &mut hsub) != 0 {
                continue;
            }
            let mut val: u32 = 0;
            let mut ty: u32 = 0;
            let mut sz = std::mem::size_of::<u32>() as u32;
            let vname = wide("ComputeSystemType");
            let q = RegQueryValueExW(
                hsub,
                vname.as_ptr(),
                std::ptr::null(),
                &mut ty,
                &mut val as *mut u32 as *mut u8,
                &mut sz,
            );
            RegCloseKey(hsub);
            if q == 0 && ty == REG_DWORD && val == 2 {
                let name_str = String::from_utf16_lossy(&name[..name_len as usize]);
                if let Some(g) = parse_guid(&name_str) {
                    found = Some(g);
                    break;
                }
            }
        }
        RegCloseKey(hkey);
        found
    }
}

/// Owns the socket; closes it when the last clone drops.
struct Sock(SOCKET);
impl Drop for Sock {
    fn drop(&mut self) {
        unsafe { closesocket(self.0) };
    }
}

/// A clonable read/write view of one vsock connection. Concurrent `recv` (reader
/// thread) and `send` (writer under the mux lock) on the same socket are safe in
/// Winsock; `shutdown` unblocks a pending `recv` for teardown.
#[derive(Clone)]
pub struct VsockHandle(Arc<Sock>);

impl VsockHandle {
    pub fn shutdown(&self) {
        unsafe { shutdown(self.0 .0, SD_BOTH) };
    }
}

impl Read for VsockHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { recv(self.0 .0, buf.as_mut_ptr(), buf.len() as i32, 0) };
        if n == SOCKET_ERROR {
            return Err(last_err());
        }
        Ok(n as usize) // 0 => EOF
    }
}

impl Write for VsockHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { send(self.0 .0, buf.as_ptr(), buf.len() as i32, 0) };
        if n == SOCKET_ERROR {
            return Err(last_err());
        }
        Ok(n as usize)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

unsafe fn set_nonblocking(s: SOCKET, on: bool) {
    let mut v: u32 = on as u32;
    ioctlsocket(s, FIONBIO, &mut v);
}

/// Connect with a timeout: a non-listening vsock port otherwise blocks the whole
/// connect-first probe. Non-blocking connect, then `select` for writable/error.
unsafe fn connect_timeout(s: SOCKET, addr: *const SOCKADDR, len: i32, ms: i32) -> io::Result<()> {
    set_nonblocking(s, true);
    if ws_connect(s, addr, len) == 0 {
        set_nonblocking(s, false);
        return Ok(());
    }
    let e = WSAGetLastError();
    if e != WSAEWOULDBLOCK {
        return Err(io::Error::from_raw_os_error(e));
    }
    let mut wfds: FD_SET = std::mem::zeroed();
    wfds.fd_count = 1;
    wfds.fd_array[0] = s;
    let mut efds: FD_SET = std::mem::zeroed();
    efds.fd_count = 1;
    efds.fd_array[0] = s;
    let tv = TIMEVAL { tv_sec: ms / 1000, tv_usec: (ms % 1000) * 1000 };
    if select(0, std::ptr::null_mut(), &mut wfds, &mut efds, &tv) <= 0 {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "vsock connect timed out"));
    }
    // Writable or errored: SO_ERROR distinguishes (0 = connected).
    let mut serr: i32 = 0;
    let mut slen = std::mem::size_of::<i32>() as i32;
    getsockopt(s, SOL_SOCKET, SO_ERROR, &mut serr as *mut i32 as *mut u8, &mut slen);
    if serr != 0 {
        return Err(io::Error::from_raw_os_error(serr));
    }
    set_nonblocking(s, false);
    Ok(())
}

/// Connect to `wslptyd` on vsock `port` in the running WSL VM.
pub fn connect(port: u32) -> io::Result<VsockHandle> {
    ensure_wsa();
    let vmid = find_wsl_vmid()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no running WSL VM"))?;
    unsafe {
        let s = socket(AF_HYPERV as i32, SOCK_STREAM, HV_PROTOCOL_RAW);
        if s == INVALID_SOCKET {
            return Err(last_err());
        }
        let addr = SockaddrHv {
            family: AF_HYPERV,
            reserved: 0,
            vm_id: vmid,
            service_id: vsock_service_guid(port),
        };
        let r = connect_timeout(
            s,
            &addr as *const SockaddrHv as *const SOCKADDR,
            std::mem::size_of::<SockaddrHv>() as i32,
            CONNECT_TIMEOUT_MS,
        );
        if let Err(e) = r {
            closesocket(s);
            return Err(e);
        }
        Ok(VsockHandle(Arc::new(Sock(s))))
    }
}

/// Connect and build a mux over the vsock socket. Dropping the mux shuts the
/// socket down (the daemon's connection child then tears down its sessions).
pub fn start_mux(port: u32) -> io::Result<(WslMux, std::sync::mpsc::Receiver<MuxEvent>)> {
    let h = connect(port)?;
    let reader = h.clone();
    let writer = h.clone();
    let teardown = h;
    Ok(WslMux::start(
        Box::new(reader),
        Box::new(writer),
        Box::new(move || teardown.shutdown()),
    ))
}
