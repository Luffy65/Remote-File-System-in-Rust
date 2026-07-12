use crate::api::{self, DirectoryEntry, RemoteMetadata};
use std::{
    collections::hash_map::DefaultHasher,
    ffi::{OsStr, c_void},
    hash::{Hash, Hasher},
    io::ErrorKind,
    sync::Mutex,
    time::{Duration, UNIX_EPOCH},
};
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

#[derive(Debug)]
struct RemoteEntry {
    kind: EntryKind,
    size: u64,
    modified_at: String,
    mode: Option<u32>,
}

struct WinHandle {
    path: Mutex<String>,
    kind: EntryKind,
    delete_on_cleanup: Mutex<bool>,
    dir_buffer: DirBuffer,
}

struct RemoteWinFs {
    server_addr: String,
    runtime: tokio::runtime::Runtime,
}

pub fn run(
    mountpoint: &str,
    server_url: &str,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let _winfsp = winfsp::winfsp_init()?;
    let fs = RemoteWinFs {
        server_addr: server_url.to_string(),
        runtime: tokio::runtime::Runtime::new()?,
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
}

impl WinHandle {
    fn new(path: String, kind: EntryKind) -> Self {
        WinHandle {
            path: Mutex::new(path),
            kind,
            delete_on_cleanup: Mutex::new(false),
            dir_buffer: DirBuffer::new(),
        }
    }

    fn path(&self) -> String {
        self.path.lock().unwrap().clone()
    }
}

impl RemoteWinFs {
    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(future)
    }

    fn api_path(path: &str) -> &str {
        path.trim_start_matches('/')
    }

    fn metadata_for_path(&self, path: &str) -> Result<RemoteEntry> {
        if path == "/" {
            return Ok(RemoteEntry::root());
        }

        let parent = parent_path(path);
        let name = file_name(path).ok_or(FspError::IO(ErrorKind::InvalidInput))?;
        let entries = self
            .block_on(api::list_directory(
                &self.server_addr,
                Self::api_path(&parent),
            ))
            .map_err(|error| fsp_error_from_api(&error))?;

        entries
            .iter()
            .find(|entry| entry.name == name)
            .map(RemoteEntry::from_directory_entry)
            .ok_or(FspError::IO(ErrorKind::NotFound))
    }

    fn fill_file_info_for_path(&self, path: &str, info: &mut FileInfo) -> Result<RemoteEntry> {
        let entry = self.metadata_for_path(path)?;
        fill_file_info(info, path, &entry);
        Ok(entry)
    }

    fn delete_path(&self, path: &str, kind: EntryKind) -> Result<()> {
        let api_path = Self::api_path(path);
        match kind {
            EntryKind::File => self
                .block_on(api::delete_file(&self.server_addr, api_path))
                .map_err(|error| fsp_error_from_api(&error)),
            EntryKind::Directory => self
                .block_on(api::delete_directory(&self.server_addr, api_path))
                .map_err(|error| fsp_error_from_api(&error)),
        }
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
                let entries = self
                    .block_on(api::list_directory(&self.server_addr, Self::api_path(path)))
                    .map_err(|error| fsp_error_from_api(&error))?;
                if !entries.is_empty() {
                    return Err(FspError::WIN32(ERROR_DIR_NOT_EMPTY));
                }
            }
        }

        Ok(())
    }
}

impl FileSystemContext for RemoteWinFs {
    type FileContext = WinHandle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Result<FileSecurity> {
        let path = normalize_winfsp_path(file_name);
        let entry = self.metadata_for_path(&path)?;

        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes: file_attributes(&entry),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let path = normalize_winfsp_path(file_name);
        let entry = self.fill_file_info_for_path(&path, file_info.as_mut())?;
        Ok(WinHandle::new(path, entry.kind))
    }

    fn close(&self, _context: Self::FileContext) {}

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let path = normalize_winfsp_path(file_name);
        let api_path = Self::api_path(&path);
        let is_directory = create_options & FILE_DIRECTORY_FILE != 0;
        let metadata = if is_directory {
            self.block_on(api::create_directory(&self.server_addr, api_path, 0o755))
                .map_err(|error| fsp_error_from_api(&error))?
        } else {
            let metadata = self
                .block_on(api::create_file(&self.server_addr, api_path, 0o644))
                .map_err(|error| fsp_error_from_api(&error))?;
            if let Some(bytes) = extra_buffer.filter(|bytes| !bytes.is_empty()) {
                self.block_on(api::write_file(&self.server_addr, api_path, bytes, 0))
                    .map_err(|error| fsp_error_from_api(&error))?
            } else {
                metadata
            }
        };
        let entry = RemoteEntry::from_metadata(&metadata);
        fill_file_info(file_info.as_mut(), &path, &entry);
        Ok(WinHandle::new(path, entry.kind))
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        let delete = *context.delete_on_cleanup.lock().unwrap()
            || FspCleanupFlags::FspCleanupDelete.is_flagged(flags);
        if delete {
            let path = context.path();
            if let Err(error) = self.delete_path(&path, context.kind) {
                log::warn!("Failed to delete {path} during cleanup: {error:?}");
            }
        }
    }

    fn flush(&self, context: Option<&Self::FileContext>, file_info: &mut FileInfo) -> Result<()> {
        if let Some(context) = context {
            let path = context.path();
            self.fill_file_info_for_path(&path, file_info)?;
        }
        Ok(())
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        let path = context.path();
        self.fill_file_info_for_path(&path, file_info)?;
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        if context.kind != EntryKind::File {
            return Err(FspError::IO(ErrorKind::IsADirectory));
        }

        let path = context.path();
        let current = self.metadata_for_path(&path)?;
        let mode = mode_after_overwrite(current.mode, file_attributes, replace_file_attributes);
        let metadata = self
            .block_on(api::overwrite_file(
                &self.server_addr,
                Self::api_path(&path),
                mode,
            ))
            .map_err(|error| fsp_error_from_api(&error))?;
        fill_file_info(file_info, &path, &RemoteEntry::from_metadata(&metadata));
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> Result<u32> {
        if context.kind != EntryKind::Directory {
            return Err(FspError::IO(ErrorKind::NotADirectory));
        }

        if marker.is_none() {
            let path = context.path();
            let entries = self
                .block_on(api::list_directory(
                    &self.server_addr,
                    Self::api_path(&path),
                ))
                .map_err(|error| fsp_error_from_api(&error))?;
            let lock = context
                .dir_buffer
                .acquire(true, Some(entries.len() as u32 + 2))?;

            let current = self.metadata_for_path(&path)?;
            write_dir_entry(&lock, ".", &path, &current)?;
            let parent = parent_path(&path);
            let parent_entry = self.metadata_for_path(&parent)?;
            write_dir_entry(&lock, "..", &parent, &parent_entry)?;

            for entry in entries {
                let child_path = child_path(&path, &entry.name);
                write_dir_entry(
                    &lock,
                    &entry.name,
                    &child_path,
                    &RemoteEntry::from_directory_entry(&entry),
                )?;
            }
        }

        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Result<()> {
        let from = normalize_winfsp_path(file_name);
        let to = normalize_winfsp_path(new_file_name);

        self.block_on(api::rename_file(
            &self.server_addr,
            Self::api_path(&from),
            Self::api_path(&to),
            replace_if_exists,
        ))
        .map_err(|error| fsp_error_from_api(&error))?;

        *context.path.lock().unwrap() = to;
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        let path = context.path();
        let mtime = system_time_from_filetime(last_write_time);
        if mtime.is_some() {
            let metadata = self
                .block_on(api::update_metadata(
                    &self.server_addr,
                    Self::api_path(&path),
                    None,
                    None,
                    None,
                    mtime,
                ))
                .map_err(|error| fsp_error_from_api(&error))?;
            fill_file_info(file_info, &path, &RemoteEntry::from_metadata(&metadata));
        } else {
            self.fill_file_info_for_path(&path, file_info)?;
        }
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<()> {
        if delete_file {
            self.validate_delete(&context.path(), context.kind)?;
        }
        *context.delete_on_cleanup.lock().unwrap() = delete_file;
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        if set_allocation_size {
            self.fill_file_info_for_path(&context.path(), file_info)?;
            return Ok(());
        }

        let path = context.path();
        let metadata = self
            .block_on(api::resize_file(
                &self.server_addr,
                Self::api_path(&path),
                new_size,
            ))
            .map_err(|error| fsp_error_from_api(&error))?;
        fill_file_info(file_info, &path, &RemoteEntry::from_metadata(&metadata));
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        if context.kind != EntryKind::File {
            return Err(FspError::IO(ErrorKind::IsADirectory));
        }

        let path = context.path();
        let bytes = self
            .block_on(api::read_file(
                &self.server_addr,
                Self::api_path(&path),
                offset,
                buffer.len() as u32,
            ))
            .map_err(|error| fsp_error_from_api(&error))?;
        let bytes_to_copy = bytes.len().min(buffer.len());
        buffer[..bytes_to_copy].copy_from_slice(&bytes[..bytes_to_copy]);
        Ok(bytes_to_copy as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Result<u32> {
        if context.kind != EntryKind::File {
            return Err(FspError::IO(ErrorKind::IsADirectory));
        }

        let path = context.path();
        let offset = if write_to_eof {
            self.metadata_for_path(&path)?.size
        } else {
            offset
        };
        let metadata = self
            .block_on(api::write_file(
                &self.server_addr,
                Self::api_path(&path),
                buffer,
                offset,
            ))
            .map_err(|error| fsp_error_from_api(&error))?;
        fill_file_info(file_info, &path, &RemoteEntry::from_metadata(&metadata));
        Ok(buffer.len() as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> Result<()> {
        out_volume_info.total_size = 1 << 40;
        out_volume_info.free_size = 1 << 39;
        out_volume_info.set_volume_label("remoteFS");
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

fn parent_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("/{parent}"),
        _ => "/".to_string(),
    }
}

fn file_name(path: &str) -> Option<String> {
    path.trim_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn child_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), name)
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
