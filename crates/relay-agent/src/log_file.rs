use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tracing_subscriber::fmt::MakeWriter;

const ROTATE_BYTES: u64 = 2 * 1024 * 1024;
const GENERATIONS: u8 = 12;

pub struct RotatingLog {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    rotate_bytes: u64,
    generations: u8,
}

impl RotatingLog {
    fn open(path: PathBuf) -> Self {
        Self::with_limits(path, ROTATE_BYTES, GENERATIONS)
    }

    pub fn with_limits(path: PathBuf, rotate_bytes: u64, generations: u8) -> Self {
        let mut log = RotatingLog {
            path,
            file: None,
            written: 0,
            rotate_bytes,
            generations,
        };
        log.reopen();
        log
    }

    fn reopen(&mut self) {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        self.file = options.open(&self.path).ok();
        self.written = self
            .file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map(|meta| meta.len())
            .unwrap_or(0);
    }

    fn generation(&self, n: u8) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{n}"));
        self.path.with_file_name(name)
    }

    fn rotate(&mut self) {
        self.file = None;
        for n in (1..=self.generations).rev() {
            let source = if n == 1 {
                self.path.clone()
            } else {
                self.generation(n - 1)
            };
            if !source.exists() {
                continue;
            }
            let target = self.generation(n);
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::rename(&source, &target);
        }
        self.reopen();
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(file) = self.file.as_mut() else {
            return Ok(buf.len());
        };
        let written = file.write(buf).unwrap_or(0);
        self.written = self.written.saturating_add(written as u64);
        if self.written >= self.rotate_bytes {
            self.rotate();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
        Ok(())
    }
}

static LOG: OnceLock<Mutex<RotatingLog>> = OnceLock::new();

#[derive(Clone, Copy)]
pub struct FileLog {
    path: &'static OnceLock<PathBuf>,
}

pub struct FileLogWriter(Option<MutexGuard<'static, RotatingLog>>);

impl Write for FileLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.as_mut() {
            Some(log) => log.write(buf),
            None => io::stderr().write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.as_mut() {
            Some(log) => log.flush(),
            None => io::stderr().flush(),
        }
    }
}

impl<'a> MakeWriter<'a> for FileLog {
    type Writer = FileLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        let Some(path) = self.path.get() else {
            return FileLogWriter(None);
        };
        let log = LOG.get_or_init(|| Mutex::new(RotatingLog::open(path.clone())));
        FileLogWriter(Some(
            log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        ))
    }
}

static PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn writer(path: &Path) -> FileLog {
    let _ = PATH.set(path.to_path_buf());
    FileLog { path: &PATH }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vsc-relay-log-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_full_log_rotates_and_keeps_writing_into_a_fresh_file() {
        let dir = temp_dir("rotate");
        let path = dir.join("agent.log");
        let mut log = RotatingLog::open(path.clone());

        let chunk = vec![b'x'; 64 * 1024];
        let mut wrote = 0u64;
        while wrote < ROTATE_BYTES + 128 * 1024 {
            log.write_all(&chunk).expect("write");
            wrote += chunk.len() as u64;
        }
        log.flush().expect("flush");

        let first = dir.join("agent.log.1");
        assert!(first.exists(), "the full generation is kept, not discarded");
        assert!(
            std::fs::metadata(&path).expect("live log").len() < ROTATE_BYTES,
            "writing continues in a fresh file rather than growing without end"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_chain_drops_its_oldest_generation_and_never_grows_past_the_cap() {
        let dir = temp_dir("depth");
        let path = dir.join("agent.log");
        let mut log = RotatingLog::open(path.clone());
        for n in 1..=GENERATIONS {
            std::fs::write(dir.join(format!("agent.log.{n}")), format!("gen{n}")).expect("seed");
        }

        log.rotate();

        assert_eq!(
            std::fs::read_to_string(dir.join(format!("agent.log.{GENERATIONS}"))).expect("read"),
            format!("gen{}", GENERATIONS - 1),
            "each generation shifts down one and the oldest is dropped"
        );
        assert!(
            !dir.join(format!("agent.log.{}", GENERATIONS + 1)).exists(),
            "rotation never invents a generation past the cap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_that_cannot_be_opened_never_fails_a_write() {
        let dir = temp_dir("unwritable");
        let mut log = RotatingLog::open(dir.join("missing-dir").join("agent.log"));
        assert!(log.file.is_none(), "the file could not be claimed");
        assert!(
            log.write(b"a line that has nowhere to go").is_ok(),
            "a log that cannot be written must never take the daemon down"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
