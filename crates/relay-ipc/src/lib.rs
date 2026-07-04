#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Hook,
    Inject(u32),
    Out(u32),
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{
    cleanup, connect_blocking, endpoint_available, live_out_pids, sweep_stale, BlockingConn,
    BlockingListener,
};
#[cfg(all(unix, feature = "async"))]
pub use unix::{connect_async, AsyncConn, AsyncListener};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    cleanup, connect_blocking, endpoint_available, live_out_pids, sweep_stale, BlockingConn,
    BlockingListener,
};
#[cfg(all(windows, feature = "async"))]
pub use windows::{connect_async, AsyncConn, AsyncListener};

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const SYNCHRONIZE: u32 = 0x0010_0000;
    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let waited = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        waited != WAIT_OBJECT_0
    }
}
