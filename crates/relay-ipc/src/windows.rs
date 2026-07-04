use crate::Endpoint;
use std::io::{self, Read, Write};
use std::os::raw::c_void;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FindClose, FindFirstFileW, FindNextFileW, ReadFile, WriteFile, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const BUFFER_SIZE: u32 = 64 * 1024;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_PIPE_NOT_CONNECTED: u32 = 233;
const SDDL_REVISION_1: u32 = 1;
const TOKEN_QUERY: u32 = 0x0008;

fn pipe_str(ep: &Endpoint) -> String {
    match ep {
        Endpoint::Hook => r"\\.\pipe\vsc-relay-hook".to_string(),
        Endpoint::Inject(pid) => format!(r"\\.\pipe\vsc-relay-inject-{pid}"),
        Endpoint::Out(pid) => format!(r"\\.\pipe\vsc-relay-out-{pid}"),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_ptr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0isize;
    unsafe {
        while *ptr.offset(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len as usize);
        String::from_utf16_lossy(slice)
    }
}

fn last_err() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

struct OwnedHandle(HANDLE);
unsafe impl Send for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Security {
    sd: *mut c_void,
    sa: SECURITY_ATTRIBUTES,
    present: bool,
}

impl Security {
    fn build() -> Self {
        let mut sec = Security {
            sd: null_mut(),
            sa: unsafe { std::mem::zeroed() },
            present: false,
        };
        if let Some(sid) = current_user_sid() {
            let sddl = wide(&format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"));
            let mut psd: *mut c_void = null_mut();
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut psd,
                    null_mut(),
                )
            };
            if ok != 0 && !psd.is_null() {
                sec.sd = psd;
                sec.sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
                sec.sa.lpSecurityDescriptor = psd;
                sec.sa.bInheritHandle = 0;
                sec.present = true;
            }
        }
        sec
    }

    fn sa_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        if self.present {
            &self.sa
        } else {
            null()
        }
    }

    fn raw(&self) -> *mut c_void {
        if self.present {
            &self.sa as *const SECURITY_ATTRIBUTES as *mut c_void
        } else {
            null_mut()
        }
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            unsafe {
                LocalFree(self.sd);
            }
        }
    }
}

fn current_user_sid() -> Option<String> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let token = OwnedHandle(token);
        let mut len = 0u32;
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        if GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr() as *mut c_void,
            len,
            &mut len,
        ) == 0
        {
            return None;
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut wsid: *mut u16 = null_mut();
        if ConvertSidToStringSidW(tu.User.Sid, &mut wsid) == 0 || wsid.is_null() {
            return None;
        }
        let s = wide_ptr_to_string(wsid);
        LocalFree(wsid as *mut c_void);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

fn create_server_instance(name: &[u16], first: bool) -> io::Result<OwnedHandle> {
    let sec = Security::build();
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            pipe_mode,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_SIZE,
            BUFFER_SIZE,
            0,
            sec.sa_ptr(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_err());
    }
    Ok(OwnedHandle(handle))
}

pub struct BlockingListener {
    name: Vec<u16>,
    pending: OwnedHandle,
}

impl BlockingListener {
    pub fn bind(ep: &Endpoint) -> io::Result<Self> {
        let name = wide(&pipe_str(ep));
        let pending = create_server_instance(&name, true)?;
        Ok(Self { name, pending })
    }

    pub fn accept(&mut self) -> io::Result<BlockingConn> {
        let ok = unsafe { ConnectNamedPipe(self.pending.0, null_mut()) };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e != ERROR_PIPE_CONNECTED {
                return Err(io::Error::from_raw_os_error(e as i32));
            }
        }
        let next = create_server_instance(&self.name, false)?;
        let connected = std::mem::replace(&mut self.pending, next);
        Ok(BlockingConn { handle: connected })
    }
}

pub struct BlockingConn {
    handle: OwnedHandle,
}

impl Read for BlockingConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                self.handle.0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                null_mut(),
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e == ERROR_BROKEN_PIPE || e == ERROR_PIPE_NOT_CONNECTED {
                return Ok(0);
            }
            return Err(io::Error::from_raw_os_error(e as i32));
        }
        Ok(read as usize)
    }
}

impl Write for BlockingConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                self.handle.0,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_err());
        }
        Ok(written as usize)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn connect_blocking(ep: &Endpoint) -> io::Result<BlockingConn> {
    let name = wide(&pipe_str(ep));
    loop {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(BlockingConn {
                handle: OwnedHandle(handle),
            });
        }
        let e = unsafe { GetLastError() };
        if e == ERROR_PIPE_BUSY {
            if unsafe { WaitNamedPipeW(name.as_ptr(), 2000) } == 0 {
                return Err(last_err());
            }
            continue;
        }
        return Err(io::Error::from_raw_os_error(e as i32));
    }
}

pub fn endpoint_available(ep: &Endpoint) -> bool {
    let name = wide(&pipe_str(ep));
    unsafe { WaitNamedPipeW(name.as_ptr(), 50) != 0 }
}

pub fn live_out_pids() -> Vec<u32> {
    let pattern = wide(r"\\.\pipe\*");
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
    if handle == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut pids = Vec::new();
    loop {
        let name = wide_ptr_to_string(data.cFileName.as_ptr());
        if let Some(rest) = name.strip_prefix("vsc-relay-out-") {
            if let Ok(pid) = rest.parse::<u32>() {
                pids.push(pid);
            }
        }
        if unsafe { FindNextFileW(handle, &mut data) } == 0 {
            break;
        }
    }
    unsafe {
        FindClose(handle);
    }
    pids
}

pub fn cleanup(_ep: &Endpoint) {}

pub fn sweep_stale() {}

#[cfg(feature = "async")]
mod asyncio {
    use super::{pipe_str, Security, ERROR_PIPE_BUSY, SECURITY_IDENTIFICATION};
    use crate::Endpoint;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    fn make_server(name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let sec = Security::build();
        let mut opts = ServerOptions::new();
        opts.reject_remote_clients(true);
        opts.first_pipe_instance(first);
        unsafe { opts.create_with_security_attributes_raw(name, sec.raw()) }
    }

    pub struct AsyncListener {
        name: String,
        server: NamedPipeServer,
    }

    impl AsyncListener {
        pub fn bind(ep: &Endpoint) -> io::Result<Self> {
            let name = pipe_str(ep);
            let server = make_server(&name, true)?;
            Ok(Self { name, server })
        }

        pub async fn accept(&mut self) -> io::Result<AsyncConn> {
            self.server.connect().await?;
            let next = make_server(&self.name, false)?;
            let connected = std::mem::replace(&mut self.server, next);
            Ok(AsyncConn::Server(connected))
        }
    }

    pub enum AsyncConn {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    impl AsyncConn {
        pub fn into_split(
            self,
        ) -> (
            tokio::io::ReadHalf<AsyncConn>,
            tokio::io::WriteHalf<AsyncConn>,
        ) {
            tokio::io::split(self)
        }
    }

    impl AsyncRead for AsyncConn {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.get_mut() {
                AsyncConn::Server(s) => Pin::new(s).poll_read(cx, buf),
                AsyncConn::Client(c) => Pin::new(c).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for AsyncConn {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                AsyncConn::Server(s) => Pin::new(s).poll_write(cx, buf),
                AsyncConn::Client(c) => Pin::new(c).poll_write(cx, buf),
            }
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                AsyncConn::Server(s) => Pin::new(s).poll_flush(cx),
                AsyncConn::Client(c) => Pin::new(c).poll_flush(cx),
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                AsyncConn::Server(s) => Pin::new(s).poll_shutdown(cx),
                AsyncConn::Client(c) => Pin::new(c).poll_shutdown(cx),
            }
        }
    }

    pub async fn connect_async(ep: &Endpoint) -> io::Result<AsyncConn> {
        let name = pipe_str(ep);
        loop {
            match ClientOptions::new()
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(&name)
            {
                Ok(client) => return Ok(AsyncConn::Client(client)),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(feature = "async")]
pub use asyncio::{connect_async, AsyncConn, AsyncListener};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn blocking_round_trip() {
        let ep = Endpoint::Inject(999_001);
        let mut listener = BlockingListener::bind(&ep).unwrap();
        let server = std::thread::spawn(move || {
            let conn = listener.accept().unwrap();
            let mut reader = BufReader::new(conn);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            reader.get_mut().write_all(b"pong\n").unwrap();
            line
        });
        let mut client = connect_blocking(&ep).unwrap();
        client.write_all(b"ping\n").unwrap();
        let mut reader = BufReader::new(client);
        let mut resp = String::new();
        reader.read_line(&mut resp).unwrap();
        assert_eq!(resp, "pong\n");
        assert_eq!(server.join().unwrap(), "ping\n");
    }

    #[test]
    fn out_pid_discovery() {
        let pid = 999_002;
        let _listener = BlockingListener::bind(&Endpoint::Out(pid)).unwrap();
        assert!(live_out_pids().contains(&pid));
        assert!(endpoint_available(&Endpoint::Out(pid)));
        assert!(!endpoint_available(&Endpoint::Out(998_002)));
    }

    #[tokio::test]
    async fn async_client_blocking_server() {
        let pid = 999_003;
        let mut listener = BlockingListener::bind(&Endpoint::Out(pid)).unwrap();
        let server = std::thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            conn.write_all(b"hello\n").unwrap();
        });
        let conn = connect_async(&Endpoint::Out(pid)).await.unwrap();
        let mut reader = tokio::io::BufReader::new(conn);
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .unwrap();
        assert_eq!(line, "hello\n");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn blocking_client_async_server() {
        let mut listener = AsyncListener::bind(&Endpoint::Hook).unwrap();
        let accept = tokio::spawn(async move {
            let conn = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(conn);
            let mut line = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                .await
                .unwrap();
            line
        });
        tokio::task::spawn_blocking(|| {
            let mut client = connect_blocking(&Endpoint::Hook).unwrap();
            client.write_all(b"req\n").unwrap();
        })
        .await
        .unwrap();
        assert_eq!(accept.await.unwrap(), "req\n");
    }
}
