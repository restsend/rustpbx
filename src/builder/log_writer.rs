use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogRotationMode {
    Never,
    Hourly,
    Daily,
}

impl LogRotationMode {
    pub fn from_config(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "hourly" => Self::Hourly,
            "daily" => Self::Daily,
            _ => Self::Never,
        }
    }

    fn period_key(self, now: chrono::DateTime<chrono::Local>) -> Option<String> {
        match self {
            Self::Hourly => Some(now.format("%Y-%m-%d-%H").to_string()),
            Self::Daily => Some(now.format("%Y-%m-%d").to_string()),
            Self::Never => None,
        }
    }
}

#[derive(Clone)]
pub struct ActiveFileRollingWriter {
    inner: Arc<parking_lot::Mutex<ActiveFileRollingWriterInner>>,
}

struct ActiveFileRollingWriterInner {
    base_path: PathBuf,
    mode: LogRotationMode,
    current_period: Option<String>,
    file: Option<File>,
}

impl ActiveFileRollingWriter {
    pub fn new(base_path: impl Into<PathBuf>, mode: LogRotationMode) -> io::Result<Self> {
        let mut inner = ActiveFileRollingWriterInner {
            base_path: base_path.into(),
            mode,
            current_period: None,
            file: None,
        };
        inner.ensure_ready()?;
        Ok(Self {
            inner: Arc::new(parking_lot::Mutex::new(inner)),
        })
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ActiveFileRollingWriter {
    type Writer = ActiveFileRollingWriteGuard;

    fn make_writer(&'a self) -> Self::Writer {
        ActiveFileRollingWriteGuard {
            inner: self.inner.clone(),
        }
    }
}

pub struct ActiveFileRollingWriteGuard {
    inner: Arc<parking_lot::Mutex<ActiveFileRollingWriterInner>>,
}

impl ActiveFileRollingWriteGuard {
    pub fn from_writer(writer: ActiveFileRollingWriter) -> Self {
        Self {
            inner: writer.inner,
        }
    }
}

impl Write for ActiveFileRollingWriteGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock();
        inner.ensure_ready()?;
        if let Some(file) = inner.file.as_mut() {
            file.write(buf)
        } else {
            Err(io::Error::other("log writer is unavailable"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = self.inner.lock();
        if let Some(file) = inner.file.as_mut() {
            file.flush()
        } else {
            Ok(())
        }
    }
}

impl ActiveFileRollingWriterInner {
    fn ensure_ready(&mut self) -> io::Result<()> {
        let now = chrono::Local::now();
        if self.mode == LogRotationMode::Never {
            if self.file.is_none() {
                self.file = Some(self.open_active_file()?);
            }
            return Ok(());
        }

        let next_period = self.mode.period_key(now).unwrap_or_default();
        let needs_rotate = self.current_period.as_deref() != Some(next_period.as_str());
        if needs_rotate {
            self.rotate(next_period)?;
        } else if self.file.is_none() {
            self.file = Some(self.open_active_file()?);
        }
        Ok(())
    }

    fn rotate(&mut self, next_period: String) -> io::Result<()> {
        if let Some(mut old_file) = self.file.take() {
            old_file.flush()?;
        }

        if let Some(ref old_period) = self.current_period
            && self.base_path.exists()
        {
            let archived = self.next_archive_path(old_period);
            fs::rename(&self.base_path, archived)?;
        }

        self.file = Some(self.open_active_file()?);
        self.current_period = Some(next_period);
        Ok(())
    }

    fn open_active_file(&self) -> io::Result<File> {
        if let Some(parent) = self.base_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.base_path)
    }

    fn next_archive_path(&self, period: &str) -> PathBuf {
        let parent = self.base_path.parent().unwrap_or_else(|| Path::new("."));
        let base_name = self
            .base_path
            .file_name()
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("rustpbx.log"));

        let mut candidate = parent.join(format!("{}.{}", base_name, period));
        if !candidate.exists() {
            return candidate;
        }

        for idx in 1..=4096 {
            candidate = parent.join(format!("{}.{}.{}", base_name, period, idx));
            if !candidate.exists() {
                return candidate;
            }
        }

        parent.join(format!("{}.{}.{}", base_name, period, 4097))
    }
}
