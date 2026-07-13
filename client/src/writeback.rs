use crate::api::{self, RemoteMetadata};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Handle;

const MANIFEST_FILE_NAME: &str = "manifest.json";
const LOCK_FILE_NAME: &str = "active.lock";
const DEFAULT_UPLOAD_CONCURRENCY: usize = 8;
static ENTRY_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingManifest {
    path: String,
    mode: u32,
}

#[derive(Debug)]
enum UploadState {
    Idle,
    Uploading,
    Committed(RemoteMetadata),
    Failed(String),
    Discarded,
}

#[derive(Debug)]
pub struct PendingFile {
    directory: PathBuf,
    manifest: PendingManifest,
    io_lock: Mutex<()>,
    current: Mutex<DataVersion>,
    state: Mutex<UploadState>,
    state_changed: Condvar,
}

#[derive(Clone, Debug)]
struct DataVersion {
    generation: u64,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct PendingFileMetadata {
    pub size: u64,
    pub modified_at: String,
    pub mode: u32,
}

struct UploadSnapshot {
    path: String,
    mode: u32,
    modified_at: SystemTime,
    file_path: PathBuf,
}

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

impl PendingFile {
    fn create(root: &Path, path: &str, mode: u32) -> io::Result<Self> {
        let id = format!(
            "{}-{}-{}",
            std::process::id(),
            unix_nanos_now(),
            ENTRY_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let directory = root.join(&id);
        let staging_directory = root.join(format!(".creating-{id}"));
        fs::create_dir(&staging_directory)?;

        let manifest = PendingManifest {
            path: path.to_string(),
            mode,
        };
        let create_result = (|| {
            let data = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(data_path(&staging_directory, 0))?;
            data.sync_all()?;
            drop(data);

            let manifest_path = staging_directory.join(MANIFEST_FILE_NAME);
            let mut manifest_file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&manifest_path)?;
            manifest_file.write_all(&serde_json::to_vec(&manifest).map_err(io::Error::other)?)?;
            manifest_file.sync_all()?;
            drop(manifest_file);
            sync_directory(&staging_directory)?;
            fs::rename(&staging_directory, &directory)?;
            sync_directory(root)
        })();
        if let Err(error) = create_result {
            let _ = fs::remove_dir_all(&staging_directory);
            let _ = fs::remove_dir_all(&directory);
            let _ = sync_directory(root);
            return Err(error);
        }

        let data_path = data_path(&directory, 0);

        Ok(PendingFile {
            directory,
            manifest,
            io_lock: Mutex::new(()),
            current: Mutex::new(DataVersion {
                generation: 0,
                path: data_path,
            }),
            state: Mutex::new(UploadState::Idle),
            state_changed: Condvar::new(),
        })
    }

    fn load(directory: PathBuf) -> io::Result<Self> {
        let manifest: PendingManifest =
            serde_json::from_slice(&fs::read(directory.join(MANIFEST_FILE_NAME))?)
                .map_err(io::Error::other)?;
        let mut committed_generations = Vec::new();
        if data_path(&directory, 0).is_file() {
            committed_generations.push(0);
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if let Some(generation) = parse_commit_generation(&entry.file_name().to_string_lossy())
                && data_path(&directory, generation).is_file()
            {
                committed_generations.push(generation);
            }
        }
        let generation = committed_generations.into_iter().max().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "journal has no fully committed data generation",
            )
        })?;
        let current_path = data_path(&directory, generation);
        remove_noncurrent_versions(&directory, generation)?;

        Ok(PendingFile {
            directory,
            manifest,
            io_lock: Mutex::new(()),
            current: Mutex::new(DataVersion {
                generation,
                path: current_path,
            }),
            state: Mutex::new(UploadState::Idle),
            state_changed: Condvar::new(),
        })
    }

    pub fn path(&self) -> &str {
        &self.manifest.path
    }

    pub fn metadata(&self) -> io::Result<PendingFileMetadata> {
        let _guard = self.io_lock.lock().unwrap();
        let metadata = fs::metadata(self.data_path())?;
        let modified_at = metadata
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        Ok(PendingFileMetadata {
            size: metadata.len(),
            modified_at,
            mode: self.manifest.mode,
        })
    }

    pub fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        let _guard = self.io_lock.lock().unwrap();
        self.require_idle()?;
        let current_path = self.data_path();
        if offset >= fs::metadata(&current_path)?.len() {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(current_path)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(bytes)?;
            return file.sync_all();
        }
        self.commit_next_version(|file| {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(bytes)
        })
    }

    pub fn resize(&self, size: u64) -> io::Result<()> {
        let _guard = self.io_lock.lock().unwrap();
        self.require_idle()?;
        let current_path = self.data_path();
        if size >= fs::metadata(&current_path)?.len() {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(current_path)?;
            file.set_len(size)?;
            return file.sync_all();
        }
        self.commit_next_version(|file| file.set_len(size))
    }

    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        let _guard = self.io_lock.lock().unwrap();
        if self.is_committed() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "pending file has already been committed",
            ));
        }
        let mut file = File::open(self.data_path())?;
        file.seek(SeekFrom::Start(offset))?;
        file.read(buffer)
    }

    pub fn is_committed(&self) -> bool {
        matches!(*self.state.lock().unwrap(), UploadState::Committed(_))
    }

    pub fn committed_metadata(&self) -> Option<RemoteMetadata> {
        match &*self.state.lock().unwrap() {
            UploadState::Committed(metadata) => Some(metadata.clone()),
            _ => None,
        }
    }

    fn require_idle(&self) -> io::Result<()> {
        match &*self.state.lock().unwrap() {
            UploadState::Idle | UploadState::Failed(_) => Ok(()),
            UploadState::Uploading => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "pending file is being uploaded",
            )),
            UploadState::Committed(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "pending file has already been committed",
            )),
            UploadState::Discarded => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "pending file was discarded",
            )),
        }
    }

    fn begin_upload(&self) -> io::Result<Option<UploadSnapshot>> {
        let _guard = self.io_lock.lock().unwrap();
        {
            let state = self.state.lock().unwrap();
            if matches!(
                *state,
                UploadState::Uploading | UploadState::Committed(_) | UploadState::Discarded
            ) {
                return Ok(None);
            }
        }

        let current_path = self.data_path();
        let modified_at = fs::metadata(&current_path)?
            .modified()
            .unwrap_or(SystemTime::now());
        *self.state.lock().unwrap() = UploadState::Uploading;

        Ok(Some(UploadSnapshot {
            path: self.manifest.path.clone(),
            mode: self.manifest.mode,
            modified_at,
            file_path: current_path,
        }))
    }

    fn finish_success(&self, metadata: RemoteMetadata) {
        *self.state.lock().unwrap() = UploadState::Committed(metadata);
        self.state_changed.notify_all();
    }

    fn finish_failure(&self, error: String) {
        *self.state.lock().unwrap() = UploadState::Failed(error);
        self.state_changed.notify_all();
    }

    fn wait_for_upload(&self) -> io::Result<RemoteMetadata> {
        let mut state = self.state.lock().unwrap();
        loop {
            match &*state {
                UploadState::Committed(metadata) => return Ok(metadata.clone()),
                UploadState::Failed(error) => return Err(io::Error::other(error.clone())),
                UploadState::Discarded => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "pending file was discarded",
                    ));
                }
                UploadState::Idle => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "pending file has not been queued",
                    ));
                }
                UploadState::Uploading => {
                    state = self.state_changed.wait(state).unwrap();
                }
            }
        }
    }

    fn discard(&self) -> io::Result<()> {
        let _guard = self.io_lock.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        if matches!(*state, UploadState::Uploading) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cannot discard a file while it is being uploaded",
            ));
        }
        *state = UploadState::Discarded;
        drop(state);
        self.state_changed.notify_all();
        fs::remove_dir_all(&self.directory)?;
        Ok(())
    }

    fn data_path(&self) -> PathBuf {
        self.current.lock().unwrap().path.clone()
    }

    fn commit_next_version(
        &self,
        mutation: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        let current = self.current.lock().unwrap().clone();
        let next_generation = current.generation.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "journal generation overflow")
        })?;
        let next_path = data_path(&self.directory, next_generation);
        fs::copy(&current.path, &next_path)?;
        let result = (|| {
            let mut next = OpenOptions::new().read(true).write(true).open(&next_path)?;
            mutation(&mut next)?;
            next.sync_all()?;
            create_commit_marker(&self.directory, next_generation)?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&next_path);
            let _ = fs::remove_file(commit_path(&self.directory, next_generation));
            return Err(error);
        }

        *self.current.lock().unwrap() = DataVersion {
            generation: next_generation,
            path: next_path,
        };
        let _ = fs::remove_file(current.path);
        let _ = fs::remove_file(commit_path(&self.directory, current.generation));
        sync_directory(&self.directory)
    }
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
            .filter(|entry| parent_path(entry.path()) == directory)
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

fn data_path(directory: &Path, generation: u64) -> PathBuf {
    directory.join(format!("data-{generation:020}.bin"))
}

fn commit_path(directory: &Path, generation: u64) -> PathBuf {
    directory.join(format!("commit-{generation:020}.ok"))
}

fn create_commit_marker(directory: &Path, generation: u64) -> io::Result<()> {
    let mut marker = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(commit_path(directory, generation))?;
    marker.write_all(generation.to_string().as_bytes())?;
    marker.sync_all()?;
    sync_directory(directory)
}

fn parse_commit_generation(name: &str) -> Option<u64> {
    name.strip_prefix("commit-")?
        .strip_suffix(".ok")?
        .parse()
        .ok()
}

fn parse_data_generation(name: &str) -> Option<u64> {
    name.strip_prefix("data-")?
        .strip_suffix(".bin")?
        .parse()
        .ok()
}

fn remove_noncurrent_versions(directory: &Path, current_generation: u64) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let generation = parse_data_generation(&name).or_else(|| parse_commit_generation(&name));
        if generation.is_some_and(|generation| generation != current_generation) {
            let _ = fs::remove_file(entry.path());
        }
    }
    sync_directory(directory)
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

fn parent_path(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &trimmed[..index],
    }
}

fn unix_nanos_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }

    #[cfg(not(windows))]
    {
        File::open(path)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remote-fs-writeback-test-{}-{}",
                std::process::id(),
                unix_nanos_now()
            ));
            fs::create_dir(&path).unwrap();
            TestDirectory(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn pending_file_is_durable_and_reloadable() {
        let root = TestDirectory::new();
        let pending = PendingFile::create(&root.0, "/docs/example.txt", 0o640).unwrap();
        pending.write_at(b"world", 6).unwrap();
        pending.write_at(b"hello ", 0).unwrap();

        let mut bytes = vec![0_u8; 11];
        assert_eq!(pending.read_at(&mut bytes, 0).unwrap(), 11);
        assert_eq!(bytes, b"hello world");
        assert_eq!(pending.metadata().unwrap().size, 11);

        let interrupted_generation = pending.current.lock().unwrap().generation + 1;
        let interrupted_path = data_path(&pending.directory, interrupted_generation);
        fs::write(&interrupted_path, b"torn update").unwrap();

        let reloaded = PendingFile::load(pending.directory.clone()).unwrap();
        assert_eq!(reloaded.path(), "/docs/example.txt");
        assert_eq!(reloaded.metadata().unwrap().mode, 0o640);
        assert!(!interrupted_path.exists());
        let snapshot = reloaded.begin_upload().unwrap().unwrap();
        assert_eq!(fs::read(snapshot.file_path).unwrap(), b"hello world");
    }

    #[test]
    fn append_and_growth_are_in_place_but_overwrites_use_new_generations() {
        let root = TestDirectory::new();
        let pending = PendingFile::create(&root.0, "/large.bin", 0o600).unwrap();

        pending.write_at(b"first", 0).unwrap();
        assert_eq!(pending.current.lock().unwrap().generation, 0);
        pending.write_at(b" second", 5).unwrap();
        assert_eq!(pending.current.lock().unwrap().generation, 0);

        pending.write_at(b"FIRST", 0).unwrap();
        assert_eq!(pending.current.lock().unwrap().generation, 1);
        pending.resize(32).unwrap();
        assert_eq!(pending.current.lock().unwrap().generation, 1);

        pending.resize(12).unwrap();
        assert_eq!(pending.current.lock().unwrap().generation, 2);
        assert_eq!(fs::read(pending.data_path()).unwrap(), b"FIRST second");
    }
}
