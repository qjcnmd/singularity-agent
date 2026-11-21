//! 结构化诊断日志：使用 `tracing` 写入 `~/.singularity/logs/singularity.log`，
//! 按体积自动滚动归档（默认单文件 5 MiB，保留 3 份历史），不阻塞或中断主流程。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use app_core::identity::{APP_SLUG, LOG_ENV_VAR, LOG_FILE_NAME};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const LOG_DIR_NAME: &str = "logs";
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const ROTATE_RETRY_BACKOFF_BYTES: u64 = 256 * 1024;
const MAX_ARCHIVES: usize = 3;

const DEFAULT_FILTER: &str = concat!(
    "info,",
    env!("CARGO_CRATE_NAME"),
    "=info,agent=info,model=info,mcp=info,hyper=warn,reqwest=warn,rustls=warn,rmcp=warn"
);

/// 初始化全局诊断日志；日志目录或文件不可写时降级静默，不影响应用启动。
pub(crate) fn init(home: &Path) {
    let dir = home.join(LOG_DIR_NAME);
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let writer = RollingLogWriter::new(dir.join(LOG_FILE_NAME));
    let filter = EnvFilter::try_from_env(LOG_ENV_VAR)
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_writer(writer)
        .try_init();
}

#[derive(Clone)]
struct RollingLogWriter {
    inner: Arc<Mutex<RollingState>>,
}

struct RollingState {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    next_rotate_at: u64,
}

impl RollingLogWriter {
    fn new(path: PathBuf) -> Self {
        let size = fs::metadata(&path).map_or(0, |meta| meta.len());
        let file = OpenOptions::new().create(true).append(true).open(&path).ok();
        Self {
            inner: Arc::new(Mutex::new(RollingState {
                path,
                file,
                size,
                next_rotate_at: MAX_LOG_BYTES,
            })),
        }
    }
}

impl RollingState {
    fn rotate_if_needed(&mut self, incoming: usize) {
        if self.size.saturating_add(incoming as u64) <= self.next_rotate_at {
            return;
        }
        self.file.take();
        let staging = self.path.with_extension("log.rotating");
        let _ = fs::remove_file(&staging);
        if fs::rename(&self.path, &staging).is_ok() {
            for index in (1..=MAX_ARCHIVES).rev() {
                let target = archive_path(&self.path, index);
                if index == MAX_ARCHIVES {
                    let _ = fs::remove_file(&target);
                } else {
                    let next = archive_path(&self.path, index + 1);
                    let _ = fs::rename(&target, &next);
                }
            }
            if fs::rename(&staging, archive_path(&self.path, 1)).is_ok() {
                self.file = OpenOptions::new().create(true).truncate(true).write(true).open(&self.path).ok();
                self.size = 0;
                self.next_rotate_at = MAX_LOG_BYTES;
                return;
            }
            let _ = fs::rename(&staging, &self.path);
        }
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        self.size = fs::metadata(&self.path).map_or(0, |meta| meta.len());
        self.next_rotate_at = self.size.saturating_add(ROTATE_RETRY_BACKOFF_BYTES);
    }

    fn write_bytes(&mut self, buf: &[u8]) {
        if self.file.is_none() {
            self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
            self.size = fs::metadata(&self.path).map_or(0, |meta| meta.len());
            if self.size < MAX_LOG_BYTES {
                self.next_rotate_at = MAX_LOG_BYTES;
            }
        }
        self.rotate_if_needed(buf.len());
        if let Some(file) = &mut self.file {
            if file.write_all(buf).is_ok() {
                self.size = self.size.saturating_add(buf.len() as u64);
            } else {
                self.file = None;
            }
        }
    }
}

fn archive_path(base: &Path, index: usize) -> PathBuf {
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or(APP_SLUG);
    let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("log");
    base.with_file_name(format!("{stem}.{index}.{ext}"))
}

struct GuardWriter {
    inner: Arc<Mutex<RollingState>>,
}

impl Write for GuardWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut state) = self.inner.lock() {
            state.write_bytes(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Ok(mut state) = self.inner.lock()
            && let Some(file) = &mut state.file
        {
            let _ = file.flush();
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for RollingLogWriter {
    type Writer = GuardWriter;

    fn make_writer(&'a self) -> Self::Writer {
        GuardWriter { inner: Arc::clone(&self.inner) }
    }
}
