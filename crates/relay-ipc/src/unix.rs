use crate::Endpoint;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

fn base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
}

fn sock_path(ep: &Endpoint) -> PathBuf {
    match ep {
        Endpoint::Hook => base_dir().join("hook.sock"),
        Endpoint::Inject(pid) => base_dir().join("inject").join(format!("{pid}.sock")),
        Endpoint::Out(pid) => base_dir().join("out").join(format!("{pid}.sock")),
    }
}

fn ensure_dir(dir: &Path) {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

fn prepare(ep: &Endpoint) -> PathBuf {
    let path = sock_path(ep);
    if let Some(dir) = path.parent() {
        ensure_dir(dir);
    }
    let _ = std::fs::remove_file(&path);
    path
}

pub fn cleanup(ep: &Endpoint) {
    let _ = std::fs::remove_file(sock_path(ep));
}

pub fn sweep_stale() {
    for sub in ["inject", "out"] {
        let dir = base_dir().join(sub);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(pid) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if !crate::process_alive(pid) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

pub fn endpoint_available(ep: &Endpoint) -> bool {
    sock_path(ep).exists()
}

pub fn live_out_pids() -> Vec<u32> {
    let dir = base_dir().join("out");
    let mut pids = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if let Some(pid) = name
                .strip_suffix(".sock")
                .and_then(|s| s.parse::<u32>().ok())
            {
                pids.push(pid);
            }
        }
    }
    pids
}

pub struct BlockingListener {
    inner: UnixListener,
}

impl BlockingListener {
    pub fn bind(ep: &Endpoint) -> io::Result<Self> {
        let path = prepare(ep);
        Ok(Self {
            inner: UnixListener::bind(path)?,
        })
    }

    pub fn accept(&mut self) -> io::Result<BlockingConn> {
        let (stream, _) = self.inner.accept()?;
        Ok(BlockingConn { inner: stream })
    }
}

pub struct BlockingConn {
    inner: UnixStream,
}

impl Read for BlockingConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for BlockingConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn connect_blocking(ep: &Endpoint) -> io::Result<BlockingConn> {
    Ok(BlockingConn {
        inner: UnixStream::connect(sock_path(ep))?,
    })
}

#[cfg(feature = "async")]
mod asyncio {
    use super::{prepare, sock_path};
    use crate::Endpoint;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::{UnixListener, UnixStream};

    pub struct AsyncListener {
        inner: UnixListener,
    }

    impl AsyncListener {
        pub fn bind(ep: &Endpoint) -> io::Result<Self> {
            let path = prepare(ep);
            Ok(Self {
                inner: UnixListener::bind(path)?,
            })
        }

        pub async fn accept(&mut self) -> io::Result<AsyncConn> {
            let (stream, _) = self.inner.accept().await?;
            Ok(AsyncConn { inner: stream })
        }
    }

    pub struct AsyncConn {
        inner: UnixStream,
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
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for AsyncConn {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    pub async fn connect_async(ep: &Endpoint) -> io::Result<AsyncConn> {
        Ok(AsyncConn {
            inner: UnixStream::connect(sock_path(ep)).await?,
        })
    }
}

#[cfg(feature = "async")]
pub use asyncio::{connect_async, AsyncConn, AsyncListener};
