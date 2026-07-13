//! Windows write-behind coordinator.
//!
//! The scheduler owns upload concurrency and conflict handling. Durable bytes,
//! generations, and crash recovery live in `journal` so neither layer needs to
//! understand the other's implementation details.

mod journal;

use crate::{
    api::{self, RemoteMetadata},
    remote_path,
};
pub(crate) use journal::PendingFile;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::runtime::Handle;

const LOCK_FILE_NAME: &str = "active.lock";
const DEFAULT_UPLOAD_CONCURRENCY: usize = 8;

struct WritebackInner {
    root: PathBuf,
    server_addr: String,
    runtime: Handle,
    upload_limit: Arc<tokio::sync::Semaphore>,
    uploads_paused: bool,
    entries: Mutex<HashMap<String, Arc<PendingFile>>>,
    _lock_file: File,
}

#[derive(Clone)]
pub struct Writeback {
    inner: Arc<WritebackInner>,
}

impl Writeback {
    pub fn new(server_addr: &str, runtime: Handle) -> io::Result<Self> {
        let root = journal_root(server_addr);
        fs::create_dir_all(&root)?;
        let lock_path = root.join(LOCK_FILE_NAME);

        #[cfg(windows)]
        let lock_file = {
            use std::os::windows::fs::OpenOptionsExt;
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .share_mode(0)
                .open(lock_path)?
        };

        #[cfg(not(windows))]
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;

        let mut entries = HashMap::new();
        for directory in fs::read_dir(&root)? {
            let directory = directory?;
            if !directory.file_type()?.is_dir() {
                continue;
            }
            if directory
                .file_name()
                .to_string_lossy()
                .starts_with(".creating-")
            {
                let _ = fs::remove_dir_all(directory.path());
                continue;
            }
            let pending = Arc::new(PendingFile::load(directory.path())?);
            if entries
                .insert(pending.path().to_string(), pending)
                .is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "journal contains duplicate paths",
                ));
            }
        }

        let concurrency = std::env::var("REMOTE_FS_UPLOAD_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_UPLOAD_CONCURRENCY);
        let uploads_paused = std::env::var("REMOTE_FS_UPLOAD_PAUSED")
            .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"));
        Ok(Writeback {
            inner: Arc::new(WritebackInner {
                root,
                server_addr: server_addr.to_string(),
                runtime,
                upload_limit: Arc::new(tokio::sync::Semaphore::new(concurrency)),
                uploads_paused,
                entries: Mutex::new(entries),
                _lock_file: lock_file,
            }),
        })
    }

    pub fn start_recovery(&self) {
        let entries: Vec<_> = self
            .inner
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        if !entries.is_empty() {
            log::warn!("Recovering {} durably journaled file(s)", entries.len());
        }
        for entry in entries {
            if let Err(error) = self.enqueue(&entry) {
                log::error!("Failed to queue recovered {}: {error}", entry.path());
            }
        }
    }

    pub fn stage_new(&self, path: &str, mode: u32) -> io::Result<Arc<PendingFile>> {
        let mut entries = self.inner.entries.lock().unwrap();
        if entries.contains_key(path) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "path is already pending",
            ));
        }
        let pending = Arc::new(PendingFile::create(&self.inner.root, path, mode)?);
        entries.insert(path.to_string(), pending.clone());
        Ok(pending)
    }

    pub fn get(&self, path: &str) -> Option<Arc<PendingFile>> {
        self.inner.entries.lock().unwrap().get(path).cloned()
    }

    pub fn entries_in_directory(&self, directory: &str) -> Vec<Arc<PendingFile>> {
        self.inner
            .entries
            .lock()
            .unwrap()
            .values()
            .filter(|entry| remote_path::parent(entry.path()) == directory)
            .cloned()
            .collect()
    }

    pub fn enqueue(&self, entry: &Arc<PendingFile>) -> io::Result<()> {
        if self.inner.uploads_paused {
            log::warn!(
                "Upload of {} is paused by REMOTE_FS_UPLOAD_PAUSED; its journal remains durable",
                entry.path()
            );
            return Ok(());
        }
        let Some(snapshot) = entry.begin_upload()? else {
            return Ok(());
        };
        let inner = self.inner.clone();
        let entry = entry.clone();
        self.inner.runtime.spawn(async move {
            let permit = match inner.upload_limit.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(error) => {
                    entry.finish_failure(format!("upload scheduler stopped: {error}"));
                    return;
                }
            };

            let result = api::conditionally_create_file_from_path(
                &inner.server_addr,
                snapshot.path.trim_start_matches('/'),
                &snapshot.file_path,
                snapshot.mode,
                snapshot.modified_at,
            )
            .await;

            let result = match result {
                Ok(metadata) => Ok(metadata),
                Err(error) if error.status() == Some(reqwest::StatusCode::PRECONDITION_FAILED) => {
                    match api::remote_file_matches_local(
                        &inner.server_addr,
                        snapshot.path.trim_start_matches('/'),
                        &snapshot.file_path,
                    )
                    .await
                    {
                        Ok(true) => api::get_metadata(
                            &inner.server_addr,
                            snapshot.path.trim_start_matches('/'),
                        )
                        .await
                        .map_err(api::UploadError::from),
                        Ok(false) => Err(api::UploadError::Io(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "remote path exists with different content; journal was retained",
                        ))),
                        Err(compare_error) => Err(compare_error),
                    }
                }
                Err(error) => Err(error),
            };
            drop(permit);

            match result {
                Ok(metadata) => {
                    entry.finish_success(metadata);
                    inner.entries.lock().unwrap().remove(entry.path());
                    let _io_guard = entry.io_lock.lock().unwrap();
                    if let Err(error) = fs::remove_dir_all(&entry.directory) {
                        log::warn!(
                            "Committed {}, but could not remove its journal: {error}",
                            entry.path()
                        );
                    }
                }
                Err(error) => {
                    log::error!("Upload of {} failed: {error}", entry.path());
                    entry.finish_failure(error.to_string());
                }
            }
        });
        Ok(())
    }

    pub fn flush(&self, entry: &Arc<PendingFile>) -> io::Result<RemoteMetadata> {
        self.enqueue(entry)?;
        entry.wait_for_upload()
    }

    pub fn flush_all(&self) -> io::Result<()> {
        let entries: Vec<_> = self
            .inner
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for entry in &entries {
            self.enqueue(entry)?;
        }
        for entry in entries {
            entry.wait_for_upload()?;
        }
        Ok(())
    }

    pub fn discard(&self, entry: &Arc<PendingFile>) -> io::Result<()> {
        entry.discard()?;
        self.inner.entries.lock().unwrap().remove(entry.path());
        Ok(())
    }
}

fn journal_root(server_addr: &str) -> PathBuf {
    let base = std::env::var_os("REMOTE_FS_JOURNAL_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join("remote-fs")
        .join("journal")
        .join(format!("server-{:016x}", stable_hash(server_addr)))
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
