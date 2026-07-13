use crate::api::{self, DirectoryEntry, RemoteMetadata};
use crate::remote_path;
use crate::writeback::{PendingFile, Writeback};
use std::{
    collections::hash_map::DefaultHasher,
    ffi::{OsStr, c_void},
    hash::{Hash, Hasher},
    io::ErrorKind,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

mod cache;
mod ops;
use cache::WindowsCache;
use winfsp::{
    FspError, Result, U16CStr,
    constants::FspCleanupFlags,
    filesystem::{
        DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
        VolumeInfo, WideNameInfo,
    },
    host::{FileSystemHost, FineGuard, VolumeParams},
};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const SECTOR_SIZE: u64 = 4096;
const UNIX_EPOCH_AS_FILETIME_SECS: u64 = 11_644_473_600;
const STATUS_UNEXPECTED_IO_ERROR: i32 = 0xC000_00E9u32 as i32;
const ERROR_DIR_NOT_EMPTY: u32 = 145;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug)]
struct RemoteEntry {
    kind: EntryKind,
    size: u64,
    modified_at: String,
    mode: Option<u32>,
}

struct WinHandle {
    path: Mutex<String>,
    kind: EntryKind,
    entry: Mutex<RemoteEntry>,
    pending: Option<Arc<PendingFile>>,
    delete_on_cleanup: Mutex<bool>,
    dir_buffer: DirBuffer,
}

struct RemoteWinFs {
    server_addr: String,
    runtime: tokio::runtime::Runtime,
    writeback: Writeback,
    cache: WindowsCache,
}

pub fn run(
    mountpoint: &str,
    server_url: &str,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let _winfsp = winfsp::winfsp_init()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let writeback = Writeback::new(server_url, runtime.handle().clone())?;
    writeback.start_recovery();
    let fs = RemoteWinFs {
        server_addr: server_url.to_string(),
        runtime,
        writeback,
        cache: WindowsCache::new(),
    };

    let mut volume = VolumeParams::new();
    volume
        .filesystem_name("remoteFS")
        .sector_size(SECTOR_SIZE as u16)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .case_sensitive_search(true)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .pass_query_directory_pattern(true)
        .file_info_timeout(1000)
        .dir_info_timeout(1000);

    let mut host: FileSystemHost<RemoteWinFs, FineGuard> = FileSystemHost::new(volume, fs)?;
    host.mount(mountpoint)?;
    host.start()?;
    log::info!("Windows filesystem mounted successfully on {mountpoint}.");

    tokio::runtime::Runtime::new()?.block_on(async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    });

    host.unmount();
    host.stop();
    Ok(())
}

impl RemoteEntry {
    fn root() -> Self {
        RemoteEntry {
            kind: EntryKind::Directory,
            size: 0,
            modified_at: "0".to_string(),
            mode: Some(0o755),
        }
    }

    fn from_directory_entry(entry: &DirectoryEntry) -> Self {
        RemoteEntry {
            kind: kind_from_type(&entry.type_),
            size: entry.size,
            modified_at: entry.modified_at.clone(),
            mode: entry.mode,
        }
    }

    fn from_metadata(metadata: &RemoteMetadata) -> Self {
        RemoteEntry {
            kind: kind_from_type(&metadata.type_),
            size: metadata.size,
            modified_at: metadata.modified_at.clone(),
            mode: metadata.mode,
        }
    }

    fn from_pending(pending: &PendingFile) -> std::io::Result<Self> {
        let metadata = pending.metadata()?;
        Ok(RemoteEntry {
            kind: EntryKind::File,
            size: metadata.size,
            modified_at: metadata.modified_at,
            mode: Some(metadata.mode),
        })
    }
}

impl WinHandle {
    fn new_with_pending(
        path: String,
        entry: RemoteEntry,
        pending: Option<Arc<PendingFile>>,
    ) -> Self {
        WinHandle {
            path: Mutex::new(path),
            kind: entry.kind,
            entry: Mutex::new(entry),
            pending,
            delete_on_cleanup: Mutex::new(false),
            dir_buffer: DirBuffer::new(),
        }
    }

    fn path(&self) -> String {
        self.path.lock().unwrap().clone()
    }

    fn entry(&self) -> RemoteEntry {
        self.entry.lock().unwrap().clone()
    }

    fn update_entry(&self, entry: RemoteEntry) {
        *self.entry.lock().unwrap() = entry;
    }
}

impl RemoteWinFs {
    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(future)
    }

    fn list_directory_cached(&self, path: &str) -> Result<Vec<DirectoryEntry>> {
        if let Some(entries) = self.cache.directory(path) {
            return Ok(entries);
        }

        let mut entries = self
            .block_on(api::list_directory(
                &self.server_addr,
                remote_path::api(path),
            ))
            .map_err(|error| fsp_error_from_api(&error))?;
        for pending in self.writeback.entries_in_directory(path) {
            if pending.is_committed() {
                continue;
            }
            let entry = remote_entry_for_pending(&pending)?;
            let name = pending.path().rsplit('/').next().unwrap_or_default();
            entries.retain(|existing| existing.name != name);
            entries.push(directory_entry_from_remote(name, &entry));
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        self.cache.insert_directory(path, entries.clone());
        Ok(entries)
    }

    fn metadata_for_path(&self, path: &str) -> Result<RemoteEntry> {
        if path == "/" {
            return Ok(RemoteEntry::root());
        }

        if let Some(pending) = self.writeback.get(path)
            && !pending.is_committed()
        {
            return remote_entry_for_pending(&pending);
        }

        if let Some(entry) = self.cache.metadata(path) {
            return Ok(entry);
        }

        if let Some(entry) = self.cache.parent_metadata(path) {
            return entry;
        }

        let metadata = self
            .block_on(api::get_metadata(&self.server_addr, remote_path::api(path)))
            .map_err(|error| fsp_error_from_api(&error))?;
        let entry = RemoteEntry::from_metadata(&metadata);
        self.cache.insert_metadata(path, &entry);
        Ok(entry)
    }

    fn fill_file_info_for_path(&self, path: &str, info: &mut FileInfo) -> Result<RemoteEntry> {
        let entry = self.metadata_for_path(path)?;
        fill_file_info(info, path, &entry);
        Ok(entry)
    }

    fn materialize_pending(&self, context: &WinHandle) -> Result<Option<RemoteEntry>> {
        let Some(pending) = &context.pending else {
            return Ok(None);
        };
        if let Some(metadata) = pending.committed_metadata() {
            let entry = RemoteEntry::from_metadata(&metadata);
            context.update_entry(entry.clone());
            return Ok(Some(entry));
        }

        let metadata = self.writeback.flush(pending).map_err(fsp_error_from_io)?;
        let entry = RemoteEntry::from_metadata(&metadata);
        context.update_entry(entry.clone());
        let path = context.path();
        self.cache.insert_metadata(&path, &entry);
        self.cache.update_directory_for_path(&path, &entry);
        Ok(Some(entry))
    }

    fn delete_path(&self, path: &str, kind: EntryKind) -> Result<()> {
        let api_path = remote_path::api(path);
        let result = match kind {
            EntryKind::File => self
                .block_on(api::delete_file(&self.server_addr, api_path))
                .map_err(|error| fsp_error_from_api(&error)),
            EntryKind::Directory => self
                .block_on(api::delete_directory(&self.server_addr, api_path))
                .map_err(|error| fsp_error_from_api(&error)),
        };
        if result.is_ok() {
            self.cache.invalidate_metadata_tree(path);
            self.cache.remove_path(path);
        }
        result
    }

    fn validate_delete(&self, path: &str, kind: EntryKind) -> Result<()> {
        if path == "/" {
            return Err(FspError::IO(ErrorKind::PermissionDenied));
        }

        match kind {
            EntryKind::File => {
                self.metadata_for_path(path)?;
            }
            EntryKind::Directory => {
                if !self.writeback.entries_in_directory(path).is_empty() {
                    return Err(FspError::WIN32(ERROR_DIR_NOT_EMPTY));
                }
                let entries = self
                    .block_on(api::list_directory(
                        &self.server_addr,
                        remote_path::api(path),
                    ))
                    .map_err(|error| fsp_error_from_api(&error))?;
                if !entries.is_empty() {
                    return Err(FspError::WIN32(ERROR_DIR_NOT_EMPTY));
                }
            }
        }

        Ok(())
    }
}

fn fsp_error_from_api(error: &reqwest::Error) -> FspError {
    match error.status().map(|status| status.as_u16()) {
        Some(400) => FspError::IO(ErrorKind::InvalidInput),
        Some(401 | 403) => FspError::IO(ErrorKind::PermissionDenied),
        Some(404) => FspError::IO(ErrorKind::NotFound),
        Some(409) => FspError::IO(ErrorKind::AlreadyExists),
        _ => FspError::NTSTATUS(STATUS_UNEXPECTED_IO_ERROR),
    }
}

fn remote_entry_for_pending(pending: &PendingFile) -> Result<RemoteEntry> {
    if let Some(metadata) = pending.committed_metadata() {
        return Ok(RemoteEntry::from_metadata(&metadata));
    }
    match RemoteEntry::from_pending(pending) {
        Ok(entry) => Ok(entry),
        Err(error) => pending
            .committed_metadata()
            .map(|metadata| RemoteEntry::from_metadata(&metadata))
            .ok_or_else(|| fsp_error_from_io(error)),
    }
}

fn fsp_error_from_io(error: std::io::Error) -> FspError {
    match error.kind() {
        ErrorKind::NotFound => FspError::IO(ErrorKind::NotFound),
        ErrorKind::AlreadyExists => FspError::IO(ErrorKind::AlreadyExists),
        ErrorKind::PermissionDenied => FspError::IO(ErrorKind::PermissionDenied),
        ErrorKind::InvalidInput => FspError::IO(ErrorKind::InvalidInput),
        _ => {
            log::error!("Local write journal error: {error}");
            FspError::NTSTATUS(STATUS_UNEXPECTED_IO_ERROR)
        }
    }
}

fn kind_from_type(type_: &str) -> EntryKind {
    if type_ == "directory" {
        EntryKind::Directory
    } else {
        EntryKind::File
    }
}

fn normalize_winfsp_path(file_name: &U16CStr) -> String {
    let normalized = file_name
        .to_string_lossy()
        .replace('\\', "/")
        .trim_matches('/')
        .to_string();
    if normalized.is_empty() {
        "/".to_string()
    } else {
        format!("/{normalized}")
    }
}

fn directory_entry_from_remote(name: &str, entry: &RemoteEntry) -> DirectoryEntry {
    DirectoryEntry {
        name: name.to_string(),
        type_: match entry.kind {
            EntryKind::File => "file",
            EntryKind::Directory => "directory",
        }
        .to_string(),
        size: entry.size,
        modified_at: entry.modified_at.clone(),
        mode: entry.mode,
        uid: None,
        gid: None,
    }
}

fn fill_file_info(info: &mut FileInfo, path: &str, entry: &RemoteEntry) {
    info.file_attributes = file_attributes(entry);
    info.allocation_size = allocation_size(entry.size);
    info.file_size = entry.size;
    let modified_at = filetime_from_unix_seconds(&entry.modified_at);
    info.creation_time = modified_at;
    info.last_access_time = modified_at;
    info.last_write_time = modified_at;
    info.change_time = modified_at;
    info.index_number = index_number(path);
}

fn file_attributes(entry: &RemoteEntry) -> u32 {
    match entry.kind {
        EntryKind::Directory => FILE_ATTRIBUTE_DIRECTORY,
        EntryKind::File => {
            let readonly = entry.mode.is_some_and(|mode| mode & 0o222 == 0);
            FILE_ATTRIBUTE_ARCHIVE | if readonly { FILE_ATTRIBUTE_READONLY } else { 0 }
        }
    }
}

fn mode_after_overwrite(
    current_mode: Option<u32>,
    file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
    replace_file_attributes: bool,
) -> Option<u32> {
    let mut mode = current_mode.unwrap_or(0o644);
    let currently_readonly = mode & 0o222 == 0;
    let requested_readonly = file_attributes & FILE_ATTRIBUTE_READONLY != 0;
    let readonly = if replace_file_attributes {
        requested_readonly
    } else {
        currently_readonly || requested_readonly
    };

    if readonly {
        mode &= !0o222;
    } else {
        mode |= 0o200;
    }

    Some(mode)
}

fn allocation_size(size: u64) -> u64 {
    if size == 0 {
        0
    } else {
        size.div_ceil(SECTOR_SIZE) * SECTOR_SIZE
    }
}

fn filetime_from_unix_seconds(value: &str) -> u64 {
    let seconds = value.parse::<u64>().unwrap_or(0);
    (seconds + UNIX_EPOCH_AS_FILETIME_SECS) * 10_000_000
}

fn system_time_from_filetime(value: u64) -> Option<std::time::SystemTime> {
    if value == 0 {
        return None;
    }
    let seconds = value / 10_000_000;
    if seconds < UNIX_EPOCH_AS_FILETIME_SECS {
        return Some(UNIX_EPOCH);
    }
    Some(UNIX_EPOCH + Duration::from_secs(seconds - UNIX_EPOCH_AS_FILETIME_SECS))
}

fn index_number(path: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

fn write_dir_entry(
    lock: &winfsp::filesystem::DirBufferLock<'_>,
    name: &str,
    path: &str,
    entry: &RemoteEntry,
) -> Result<()> {
    let mut dir_info = DirInfo::<255>::new();
    fill_file_info(dir_info.file_info_mut(), path, entry);
    dir_info.set_name(OsStr::new(name))?;
    lock.write(&mut dir_info)
}
