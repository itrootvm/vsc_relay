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
static SINGLE_INSTANCE: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

#[cfg(unix)]
pub fn single_instance_lock_path(name: &str) -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".vsc-relay")
        .join(format!("{name}.lock"))
}

#[cfg(unix)]
pub fn acquire_single_instance(name: &str) -> bool {
    if SINGLE_INSTANCE.get().is_some() {
        return true;
    }
    let path = single_instance_lock_path(name);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    else {
        return true;
    };
    match claim_lock_file(&mut file) {
        true => {
            let _ = SINGLE_INSTANCE.set(file);
            true
        }
        false => false,
    }
}

#[cfg(unix)]
pub fn claim_lock_file(file: &mut std::fs::File) -> bool {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return false;
    }
    let _ = file.set_len(0);
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
    true
}

#[cfg(unix)]
pub fn single_instance_holder(name: &str) -> Option<u32> {
    std::fs::read_to_string(single_instance_lock_path(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(windows)]
pub fn single_instance_holder(_name: &str) -> Option<u32> {
    None
}

#[cfg(windows)]
pub fn acquire_single_instance(name: &str) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, wide.as_ptr());
        if handle.is_null() {
            return true;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            false
        } else {
            true
        }
    }
}

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

#[cfg(all(test, unix))]
mod single_instance_tests {
    use super::*;

    fn open(path: &std::path::Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .expect("open lock file")
    }

    #[test]
    fn a_second_relay_cannot_take_a_lock_the_first_one_holds() {
        let path = std::env::temp_dir().join(format!("vsc-relay-lock-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut first = open(&path);
        assert!(
            claim_lock_file(&mut first),
            "the first relay takes the lock"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string(),
            "the holder stamps its pid so the loser can name it"
        );

        let mut second = open(&path);
        assert!(
            !claim_lock_file(&mut second),
            "two relays must never both hold it, that is how one bot gets polled twice"
        );

        drop(first);
        let mut third = open(&path);
        assert!(
            claim_lock_file(&mut third),
            "a released lock is free again, even if the holder was killed outright"
        );
        drop(third);
        let _ = std::fs::remove_file(&path);
    }
}
