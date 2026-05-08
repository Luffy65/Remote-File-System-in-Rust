use fuser::{
    FUSE_ROOT_ID, FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, Request,
};
use libc::{ENOENT, c_int};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api; // Assuming api.rs is in src/api.rs
use crate::cache::TtlLruCache;

const TTL: Duration = Duration::from_secs(1); // Kernel attribute TTL.
const ATTR_CACHE_TTL: Duration = Duration::from_secs(5);
const DIRECTORY_CACHE_TTL: Duration = Duration::from_secs(5);
const DIRECTORY_CACHE_MAX_ENTRIES: usize = 256;

// Helper function to create FileAttr
fn create_file_attr(
    ino: u64,
    kind: FileType,
    size: u64,
    perm: u16,
    uid: u32,
    gid: u32,
    modified_at: SystemTime,
) -> FileAttr {
    FileAttr {
        ino,
        size,
        blocks: (size + 511) / 512, // Calculate blocks assuming 512 byte block size
        atime: modified_at,
        mtime: modified_at,
        ctime: modified_at,
        crtime: modified_at,
        kind,
        perm,
        nlink: if kind == FileType::Directory { 2 } else { 1 }, // Directories usually have nlink 2 (for . and ..)
        uid,
        gid,
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

fn default_perm(kind: FileType) -> u16 {
    if kind == FileType::Directory {
        0o755
    } else {
        0o644
    }
}

fn system_time_from_unix_seconds(value: &str) -> SystemTime {
    let seconds = value.parse::<u64>().unwrap_or(0);
    UNIX_EPOCH + Duration::from_secs(seconds)
}

fn kind_from_type(type_: &str) -> FileType {
    if type_ == "directory" {
        FileType::Directory
    } else {
        FileType::RegularFile
    }
}

fn attr_from_directory_entry(ino: u64, entry: &api::DirectoryEntry) -> FileAttr {
    let kind = kind_from_type(&entry.type_);
    let perm = entry
        .mode
        .map(|mode| (mode & 0o7777) as u16)
        .unwrap_or_else(|| default_perm(kind));
    let modified_at = system_time_from_unix_seconds(&entry.modified_at);

    create_file_attr(
        ino,
        kind,
        entry.size,
        perm,
        entry.uid.unwrap_or(0),
        entry.gid.unwrap_or(0),
        modified_at,
    )
}

fn attr_from_remote_metadata(ino: u64, metadata: &api::RemoteMetadata) -> FileAttr {
    let kind = kind_from_type(&metadata.type_);
    let perm = metadata
        .mode
        .map(|mode| (mode & 0o7777) as u16)
        .unwrap_or_else(|| default_perm(kind));
    let modified_at = system_time_from_unix_seconds(&metadata.modified_at);

    create_file_attr(
        ino,
        kind,
        metadata.size,
        perm,
        metadata.uid.unwrap_or(0),
        metadata.gid.unwrap_or(0),
        modified_at,
    )
}

fn apply_umask(mode: u32, umask: u32, fallback: u16) -> u32 {
    let effective_mode = (mode & !umask) & 0o7777;
    if effective_mode == 0 {
        fallback as u32
    } else {
        effective_mode
    }
}

fn time_or_now(time: fuser::TimeOrNow) -> SystemTime {
    match time {
        fuser::TimeOrNow::SpecificTime(time) => time,
        fuser::TimeOrNow::Now => SystemTime::now(),
    }
}

#[derive(Debug, Clone)]
struct CachedAttr {
    attr: FileAttr,
    refreshed_at: Instant,
}

impl CachedAttr {
    fn new(attr: FileAttr) -> Self {
        CachedAttr {
            attr,
            refreshed_at: Instant::now(),
        }
    }

    fn is_fresh(&self) -> bool {
        self.attr.ino == FUSE_ROOT_ID || self.refreshed_at.elapsed() <= ATTR_CACHE_TTL
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandleKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
struct OpenHandle {
    ino: u64,
    path: String,
    kind: HandleKind,
    flags: i32,
    dirty: bool,
}

#[derive(Debug)]
pub struct RemoteFs {
    server_addr: String,
    runtime: Arc<tokio::runtime::Runtime>,
    inode_map: Arc<Mutex<HashMap<u64, CachedAttr>>>,
    path_to_inode: Arc<Mutex<HashMap<String, u64>>>,
    next_inode: Arc<Mutex<u64>>,
    directory_cache: Arc<Mutex<TtlLruCache<String, Vec<api::DirectoryEntry>>>>,
    open_handles: Arc<Mutex<HashMap<u64, OpenHandle>>>,
    next_handle: Arc<Mutex<u64>>,
}

impl RemoteFs {
    pub fn new(server_addr: &str) -> Self {
        let rt = Arc::new(tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime"));
        let mut inode_map_val = HashMap::new();
        let mut path_to_inode_val = HashMap::new();

        let uid = unsafe { libc::getuid() as u32 };
        let gid = unsafe { libc::getgid() as u32 };

        // Add root directory
        let root_attr = create_file_attr(
            FUSE_ROOT_ID,
            FileType::Directory,
            0,
            0o755,
            uid,
            gid,
            SystemTime::now(),
        );
        inode_map_val.insert(FUSE_ROOT_ID, CachedAttr::new(root_attr));
        path_to_inode_val.insert("/".to_string(), FUSE_ROOT_ID);

        info!(
            "RemoteFs initialized. Root inode: {}, Path: /",
            FUSE_ROOT_ID
        );

        RemoteFs {
            server_addr: server_addr.to_string(),
            runtime: rt,
            inode_map: Arc::new(Mutex::new(inode_map_val)),
            path_to_inode: Arc::new(Mutex::new(path_to_inode_val)),
            next_inode: Arc::new(Mutex::new(FUSE_ROOT_ID + 1)),
            directory_cache: Arc::new(Mutex::new(TtlLruCache::new(
                DIRECTORY_CACHE_MAX_ENTRIES,
                DIRECTORY_CACHE_TTL,
            ))),
            open_handles: Arc::new(Mutex::new(HashMap::new())),
            next_handle: Arc::new(Mutex::new(1)),
        }
    }

    // Finds the cached path that belongs to an inode.
    fn path_for_inode(&self, ino: u64) -> Option<String> {
        let path_map = self.path_to_inode.lock().unwrap();
        path_map.iter().find_map(|(path, &inode)| {
            if inode == ino {
                Some(path.clone())
            } else {
                None
            }
        })
    }

    // Builds a full child path from a parent inode and a file name.
    fn child_path(&self, parent: u64, name: &OsStr) -> Result<String, c_int> {
        let name_str = name.to_str().ok_or(libc::EINVAL)?;
        let parent_path = self.path_for_inode(parent).ok_or(ENOENT)?;

        if parent_path == "/" {
            Ok(format!("/{}", name_str))
        } else {
            Ok(format!(
                "{}/{}",
                parent_path.trim_end_matches('/'),
                name_str
            ))
        }
    }

    fn parent_path(path: &str) -> String {
        let parent = Path::new(path).parent().unwrap_or_else(|| Path::new("/"));
        parent.to_str().unwrap_or("/").to_string()
    }

    fn directory_cache_key(path: &str) -> String {
        if path == "/" || path.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", path.trim_matches('/'))
        }
    }

    fn api_path(path: &str) -> String {
        if path == "/" {
            "".to_string()
        } else {
            path.trim_start_matches('/').to_string()
        }
    }

    fn allocate_inode(&self) -> u64 {
        let mut next_inode = self.next_inode.lock().unwrap();
        let ino = *next_inode;
        *next_inode += 1;
        ino
    }

    fn allocate_handle(&self, ino: u64, path: String, kind: HandleKind, flags: i32) -> u64 {
        let mut next_handle = self.next_handle.lock().unwrap();
        let handle = *next_handle;
        *next_handle += 1;

        self.open_handles.lock().unwrap().insert(
            handle,
            OpenHandle {
                ino,
                path,
                kind,
                flags,
                dirty: false,
            },
        );

        handle
    }

    fn handle_for(&self, fh: u64, ino: u64, kind: HandleKind) -> Result<OpenHandle, c_int> {
        if fh == 0 {
            return Err(libc::EBADF);
        }

        let handles = self.open_handles.lock().unwrap();
        match handles.get(&fh) {
            Some(handle) if handle.ino == ino && handle.kind == kind => Ok(handle.clone()),
            Some(_) => Err(libc::EBADF),
            None => Err(libc::EBADF),
        }
    }

    fn mark_handle_dirty(&self, fh: u64) {
        if let Some(handle) = self.open_handles.lock().unwrap().get_mut(&fh) {
            handle.dirty = true;
        }
    }

    fn release_handle(&self, fh: u64, kind: HandleKind) -> Result<OpenHandle, c_int> {
        let mut handles = self.open_handles.lock().unwrap();
        match handles.remove(&fh) {
            Some(handle) if handle.kind == kind => Ok(handle),
            Some(handle) => {
                handles.insert(fh, handle);
                Err(libc::EBADF)
            }
            None => Err(libc::EBADF),
        }
    }

    fn cache_attr(&self, path: String, attr: FileAttr) {
        self.inode_map
            .lock()
            .unwrap()
            .insert(attr.ino, CachedAttr::new(attr));
        self.path_to_inode.lock().unwrap().insert(path, attr.ino);
    }

    fn update_cached_attr(&self, ino: u64, attr: FileAttr) {
        self.inode_map
            .lock()
            .unwrap()
            .insert(ino, CachedAttr::new(attr));
    }

    fn attr_for_inode(&self, ino: u64) -> Option<FileAttr> {
        self.inode_map
            .lock()
            .unwrap()
            .get(&ino)
            .map(|cached| cached.attr)
    }

    fn fresh_attr_for_inode(&self, ino: u64) -> Option<FileAttr> {
        self.inode_map
            .lock()
            .unwrap()
            .get(&ino)
            .filter(|cached| cached.is_fresh())
            .map(|cached| cached.attr)
    }

    fn list_directory_cached(
        &self,
        directory_path: &str,
    ) -> Result<Vec<api::DirectoryEntry>, reqwest::Error> {
        let cache_key = Self::directory_cache_key(directory_path);
        if let Some(entries) = self.directory_cache.lock().unwrap().get(&cache_key) {
            debug!("DIRECTORY CACHE HIT: {}", cache_key);
            return Ok(entries);
        }

        debug!("DIRECTORY CACHE MISS: {}", cache_key);
        let api_path = Self::api_path(&cache_key);
        let entries = self
            .runtime
            .block_on(api::list_directory(&self.server_addr, &api_path))?;

        self.directory_cache
            .lock()
            .unwrap()
            .insert(cache_key, entries.clone());

        Ok(entries)
    }

    fn invalidate_directory_cache_for_path(&self, path: &str) {
        let directory_path = if path == "/" {
            "/".to_string()
        } else {
            Self::parent_path(path)
        };
        let directory_key = Self::directory_cache_key(&directory_path);

        self.directory_cache.lock().unwrap().remove(&directory_key);
    }

    fn invalidate_directory_cache_tree(&self, path: &str) {
        let path_key = Self::directory_cache_key(path);
        let parent_key = Self::directory_cache_key(&Self::parent_path(path));
        self.directory_cache.lock().unwrap().remove_matching(|key| {
            key == &path_key
                || key == &parent_key
                || key.starts_with(&format!("{}/", path_key.trim_end_matches('/')))
        });
    }

    fn attr_from_entry_for_path(&self, path: &str, entry: &api::DirectoryEntry) -> FileAttr {
        let ino = self
            .path_to_inode
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or_else(|| self.allocate_inode());
        attr_from_directory_entry(ino, entry)
    }

    fn refresh_path_from_parent(&self, path: &str) -> Option<FileAttr> {
        if path == "/" {
            return self.attr_for_inode(FUSE_ROOT_ID);
        }

        let parent_path = Self::parent_path(path);
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_string())?;

        match self.list_directory_cached(&parent_path) {
            Ok(entries) => entries
                .into_iter()
                .find(|entry| entry.name == name)
                .map(|entry| {
                    let attr = self.attr_from_entry_for_path(path, &entry);
                    self.cache_attr(path.to_string(), attr);
                    attr
                }),
            Err(err) => {
                warn!("Failed to refresh metadata for {}: {:?}", path, err);
                None
            }
        }
    }

    fn refresh_inode_from_parent(&self, ino: u64) -> Option<FileAttr> {
        let path = self.path_for_inode(ino)?;
        self.refresh_path_from_parent(&path)
    }

    // Resolves rename sources, including macOS cases where macFUSE reports a truncated name.
    fn resolve_rename_source_path(
        &self,
        parent: u64,
        name: &OsStr,
        newname: &OsStr,
    ) -> Result<String, c_int> {
        let full_path = self.child_path(parent, name)?;

        {
            let path_to_inode = self.path_to_inode.lock().unwrap();
            if path_to_inode.contains_key(&full_path) {
                return Ok(full_path);
            }
        }

        let name_str = name.to_str().ok_or(libc::EINVAL)?;
        let newname_str = newname.to_str().ok_or(libc::EINVAL)?;
        let wants_appledouble = newname_str.starts_with("._");
        let parent_path = self.path_for_inode(parent).ok_or(ENOENT)?;
        let parent_prefix = if parent_path == "/" {
            "/".to_string()
        } else {
            format!("{}/", parent_path.trim_end_matches('/'))
        };

        let path_to_inode = self.path_to_inode.lock().unwrap();
        let mut candidates: Vec<String> = path_to_inode
            .keys()
            .filter_map(|path| {
                let child_name = path.strip_prefix(&parent_prefix)?;
                if child_name.contains('/') || !child_name.ends_with(name_str) {
                    return None;
                }
                Some(path.clone())
            })
            .collect();

        candidates.retain(|path| {
            path.rsplit('/')
                .next()
                .is_some_and(|child_name| child_name.starts_with("._") == wants_appledouble)
        });

        if candidates.len() == 1 {
            Ok(candidates.remove(0))
        } else {
            Ok(full_path)
        }
    }

    // Removes a cached path and all cached children below it.
    fn remove_cached_path(&self, path: &str) {
        let mut inode_map = self.inode_map.lock().unwrap();
        let mut path_to_inode = self.path_to_inode.lock().unwrap();
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let removed_paths: Vec<String> = path_to_inode
            .keys()
            .filter(|cached_path| *cached_path == path || cached_path.starts_with(&prefix))
            .cloned()
            .collect();

        for removed_path in removed_paths {
            if let Some(ino) = path_to_inode.remove(&removed_path) {
                inode_map.remove(&ino);
            }
        }

        drop(path_to_inode);
        drop(inode_map);
        self.invalidate_directory_cache_tree(path);
    }

    // Moves cached paths from one prefix to another after a successful server rename.
    fn rename_cached_path(&self, from: &str, to: &str) {
        let mut inode_map = self.inode_map.lock().unwrap();
        let mut path_to_inode = self.path_to_inode.lock().unwrap();
        let prefix = format!("{}/", from.trim_end_matches('/'));
        let moved_paths: Vec<(String, u64)> = path_to_inode
            .iter()
            .filter(|(cached_path, _)| *cached_path == from || cached_path.starts_with(&prefix))
            .map(|(cached_path, &ino)| (cached_path.clone(), ino))
            .collect();

        let destination_prefix = format!("{}/", to.trim_end_matches('/'));
        let overwritten_paths: Vec<String> = path_to_inode
            .keys()
            .filter(|cached_path| {
                *cached_path == to || cached_path.starts_with(&destination_prefix)
            })
            .cloned()
            .collect();

        for overwritten_path in overwritten_paths {
            if let Some(ino) = path_to_inode.remove(&overwritten_path) {
                inode_map.remove(&ino);
            }
        }

        for (old_path, _) in &moved_paths {
            path_to_inode.remove(old_path);
        }

        for (old_path, ino) in moved_paths {
            let new_path = if old_path == from {
                to.to_string()
            } else {
                let suffix = old_path.strip_prefix(&prefix).unwrap_or("");
                format!("{}/{}", to.trim_end_matches('/'), suffix)
            };
            path_to_inode.insert(new_path, ino);
        }

        drop(path_to_inode);
        drop(inode_map);

        let mut open_handles = self.open_handles.lock().unwrap();
        for handle in open_handles.values_mut() {
            if handle.path == from || handle.path.starts_with(&prefix) {
                handle.path = if handle.path == from {
                    to.to_string()
                } else {
                    let suffix = handle.path.strip_prefix(&prefix).unwrap_or("");
                    format!("{}/{}", to.trim_end_matches('/'), suffix)
                };
            }
        }
        drop(open_handles);

        self.invalidate_directory_cache_tree(from);
        self.invalidate_directory_cache_tree(to);
    }
}

impl Filesystem for RemoteFs {
    fn init(&mut self, _req: &Request<'_>, _config: &mut fuser::KernelConfig) -> Result<(), c_int> {
        info!("Filesystem init method called.");
        Ok(())
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(libc::EINVAL); // Invalid argument
                return;
            }
        };
        debug!(
            "mkdir(parent={}, name='{}', mode={:o})",
            parent, name_str, mode
        );

        let full_path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(err) => {
                reply.error(err);
                return;
            }
        };

        let api_path = full_path.trim_start_matches('/');
        let effective_mode = apply_umask(mode, _umask, 0o755);

        // Tell the remote server to create it and return real metadata.
        match self.runtime.block_on(api::create_directory(
            &self.server_addr,
            api_path,
            effective_mode,
        )) {
            Ok(metadata) => {
                let new_ino = self.allocate_inode();
                let attr = attr_from_remote_metadata(new_ino, &metadata);
                self.cache_attr(full_path.clone(), attr);
                self.invalidate_directory_cache_for_path(&full_path);

                debug!(
                    "Successfully created and cached new directory: ino={}, path='{}'",
                    new_ino, full_path
                );

                reply.entry(&TTL, &attr, 0);
            }
            Err(err) => {
                error!(
                    "Failed to create directory {} on server: {:?}",
                    api_path, err
                );
                reply.error(libc::EIO); // I/O Error
            }
        }
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        debug!("open(ino={}, flags={})", ino, _flags);

        if let Some(attr) = self.attr_for_inode(ino) {
            if attr.kind == FileType::RegularFile {
                match self.path_for_inode(ino) {
                    Some(path) => {
                        let fh = self.allocate_handle(ino, path, HandleKind::File, _flags);
                        reply.opened(fh, 0);
                    }
                    None => reply.error(ENOENT),
                }
            } else {
                reply.error(libc::EISDIR);
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn opendir(&mut self, _req: &Request, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        debug!("opendir(ino={}, flags={})", ino, _flags);

        if let Some(attr) = self.attr_for_inode(ino) {
            if attr.kind == FileType::Directory {
                match self.path_for_inode(ino) {
                    Some(path) => {
                        let fh = self.allocate_handle(ino, path, HandleKind::Directory, _flags);
                        reply.opened(fh, 0);
                    }
                    None => reply.error(ENOENT),
                }
            } else {
                reply.error(libc::ENOTDIR);
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        debug!("flush(ino={}, fh={})", ino, fh);

        match self.handle_for(fh, ino, HandleKind::File) {
            Ok(handle) => {
                debug!(
                    "flush accepted for path='{}', dirty={}",
                    handle.path, handle.dirty
                );
                reply.ok();
            }
            Err(err) => reply.error(err),
        }
    }

    fn fsync(&mut self, _req: &Request<'_>, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        debug!("fsync(ino={}, fh={})", ino, fh);

        match self.handle_for(fh, ino, HandleKind::File) {
            Ok(_) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        debug!("release(ino={}, fh={}, flags={})", ino, fh, _flags);

        match self.release_handle(fh, HandleKind::File) {
            Ok(handle) if handle.ino == ino => {
                debug!(
                    "Released file handle {} for path='{}', dirty={}",
                    fh, handle.path, handle.dirty
                );
                reply.ok();
            }
            Ok(handle) => {
                self.open_handles.lock().unwrap().insert(fh, handle);
                reply.error(libc::EBADF);
            }
            Err(err) => reply.error(err),
        }
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        debug!("releasedir(ino={}, fh={}, flags={})", ino, fh, _flags);

        match self.release_handle(fh, HandleKind::Directory) {
            Ok(handle) if handle.ino == ino => reply.ok(),
            Ok(handle) => {
                self.open_handles.lock().unwrap().insert(fh, handle);
                reply.error(libc::EBADF);
            }
            Err(err) => reply.error(err),
        }
    }

    fn destroy(&mut self) {
        self.open_handles.lock().unwrap().clear();
        self.directory_cache.lock().unwrap().clear();
        info!("Filesystem destroyed.");
    }

    fn lookup(&mut self, _req: &Request, parent_ino: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        debug!("lookup(parent_ino={}, name='{}')", parent_ino, name_str);

        // 1. Determine the parent path
        let parent_path_str = {
            let path_to_inode_map = self.path_to_inode.lock().unwrap();
            path_to_inode_map
                .iter()
                .find_map(|(path, &inode)| {
                    if inode == parent_ino {
                        Some(path.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| {
                    warn!(
                        "Parent path for ino {} not found, defaulting to root.",
                        parent_ino
                    );
                    "/".to_string()
                })
        };

        let full_path = if parent_path_str == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path_str.trim_end_matches('/'), name_str)
        };

        debug!("Looking up full path: {}", full_path);

        // 2. Check the local inode cache first, but only while metadata is fresh.
        {
            let path_to_inode_map = self.path_to_inode.lock().unwrap();
            if let Some(&ino) = path_to_inode_map.get(&full_path) {
                if let Some(attr) = self.fresh_attr_for_inode(ino) {
                    debug!("CACHE HIT: Found attr for path {}: {:?}", full_path, attr);
                    reply.entry(&TTL, &attr, 0);
                    return;
                }
            }
        }

        // 3. Cache miss or expired metadata: ask the server about the parent directory.
        debug!(
            "CACHE MISS: Fetching parent directory '{}' from server...",
            parent_path_str
        );

        match self.list_directory_cached(&parent_path_str) {
            Ok(api_entries) => {
                if let Some(api_entry) = api_entries.into_iter().find(|e| e.name == name_str) {
                    let attr = self.attr_from_entry_for_path(&full_path, &api_entry);
                    self.cache_attr(full_path.clone(), attr);

                    debug!(
                        "Dynamically added new entry to cache: ino={}, path='{}'",
                        attr.ino, full_path
                    );
                    reply.entry(&TTL, &attr, 0);
                } else {
                    debug!("Path {} genuinely does not exist on the server.", full_path);
                    reply.error(ENOENT);
                }
            }
            Err(err) => {
                error!("Network error during lookup of {}: {:?}", full_path, err);
                reply.error(ENOENT);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        debug!("getattr(ino={})", ino);

        if let Some(attr) = self.fresh_attr_for_inode(ino) {
            reply.attr(&TTL, &attr);
            return;
        }

        match self
            .refresh_inode_from_parent(ino)
            .or_else(|| self.attr_for_inode(ino))
        {
            Some(attr) => reply.attr(&TTL, &attr),
            None => {
                warn!("getattr: Inode {} not found in map.", ino);
                reply.error(ENOENT);
            }
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        debug!(
            "create(parent={}, name='{}', mode={:o})",
            parent, name_str, mode
        );

        let full_path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(err) => {
                reply.error(err);
                return;
            }
        };
        let api_path = full_path.trim_start_matches('/');
        let effective_mode = apply_umask(mode, _umask, 0o644);

        // Tell the remote server to initialize the empty file and return real metadata.
        match self.runtime.block_on(api::create_file(
            &self.server_addr,
            api_path,
            effective_mode,
        )) {
            Ok(metadata) => {
                let new_ino = self.allocate_inode();
                let attr = attr_from_remote_metadata(new_ino, &metadata);
                self.cache_attr(full_path.clone(), attr);
                self.invalidate_directory_cache_for_path(&full_path);
                let fh = self.allocate_handle(new_ino, full_path.clone(), HandleKind::File, _flags);

                debug!(
                    "Successfully created file: ino={}, path='{}'",
                    new_ino, full_path
                );

                reply.created(&TTL, &attr, 0, fh, 0);
            }
            Err(err) => {
                error!("Failed to create file {} on server: {:?}", api_path, err);
                reply.error(libc::EIO);
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        debug!("write(ino={}, offset={}, size={})", ino, offset, data.len());

        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }

        let file_path = if _fh != 0 {
            match self.handle_for(_fh, ino, HandleKind::File) {
                Ok(handle) => handle.path,
                Err(err) => {
                    reply.error(err);
                    return;
                }
            }
        } else {
            match self.path_for_inode(ino) {
                Some(path) => path,
                None => {
                    error!("write: Could not find path for ino {}", ino);
                    reply.error(ENOENT);
                    return;
                }
            }
        };
        let api_path = file_path.trim_start_matches('/');

        // Send the chunk of bytes to the server.
        match self.runtime.block_on(api::write_file(
            &self.server_addr,
            api_path,
            data,
            offset as u64,
        )) {
            Ok(metadata) => {
                let attr = attr_from_remote_metadata(ino, &metadata);
                self.update_cached_attr(ino, attr);
                self.invalidate_directory_cache_for_path(&file_path);
                if _fh != 0 {
                    self.mark_handle_dirty(_fh);
                }

                reply.written(data.len() as u32);
            }
            Err(err) => {
                error!("Failed to write to file {} on server: {:?}", api_path, err);
                reply.error(libc::EIO);
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        debug!("setattr(ino={}, size={:?})", ino, size);

        let path = match self.path_for_inode(ino) {
            Some(path) => path,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        let api_path = path.trim_start_matches('/');
        let mut latest_metadata = None;

        // If the OS resizes a file, mirror that size change on the server.
        if let Some(s) = size {
            match self
                .runtime
                .block_on(api::resize_file(&self.server_addr, api_path, s))
            {
                Ok(metadata) => latest_metadata = Some(metadata),
                Err(err) => {
                    error!("Failed to resize file {} on server: {:?}", api_path, err);
                    reply.error(libc::EIO);
                    return;
                }
            }
        }

        let requested_mtime = _mtime.map(time_or_now);
        if mode.is_some() || uid.is_some() || gid.is_some() || requested_mtime.is_some() {
            match self.runtime.block_on(api::update_metadata(
                &self.server_addr,
                api_path,
                mode.map(|mode| mode & 0o7777),
                uid,
                gid,
                requested_mtime,
            )) {
                Ok(metadata) => latest_metadata = Some(metadata),
                Err(err) => {
                    error!("Failed to update metadata for {}: {:?}", api_path, err);
                    reply.error(libc::EIO);
                    return;
                }
            }
        }

        if let Some(metadata) = latest_metadata {
            let attr = attr_from_remote_metadata(ino, &metadata);
            self.update_cached_attr(ino, attr);
            self.invalidate_directory_cache_for_path(&path);
            reply.attr(&TTL, &attr);
        } else if let Some(attr) = self.attr_for_inode(ino) {
            reply.attr(&TTL, &attr);
        } else {
            reply.error(ENOENT);
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        debug!("read(ino={}, offset={}, size={})", ino, offset, size);

        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }

        let file_path = if _fh != 0 {
            match self.handle_for(_fh, ino, HandleKind::File) {
                Ok(handle) => handle.path,
                Err(err) => {
                    reply.error(err);
                    return;
                }
            }
        } else {
            match self.path_for_inode(ino) {
                Some(path) => path,
                None => {
                    error!("read: Could not find path for ino {}", ino);
                    reply.error(ENOENT);
                    return;
                }
            }
        };
        let api_path = file_path.trim_start_matches('/');

        // Fetch only the byte range requested by the kernel.
        match self.runtime.block_on(api::read_file(
            &self.server_addr,
            api_path,
            offset as u64,
            size,
        )) {
            Ok(bytes) => {
                reply.data(&bytes);
            }
            Err(err) => {
                error!("Failed to read file {} from server: {:?}", api_path, err);
                reply.error(libc::EIO); // I/O Error
            }
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        // Resolve the file path, ask the server to delete it, then clear the local cache.
        let full_path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(err) => {
                reply.error(err);
                return;
            }
        };

        debug!("unlink(parent={}, path='{}')", parent, full_path);

        // `unlink` must reject directories; those should go through `rmdir`.
        if let Some(ino) = self.path_to_inode.lock().unwrap().get(&full_path).copied() {
            if let Some(attr) = self.inode_map.lock().unwrap().get(&ino) {
                if attr.attr.kind == FileType::Directory {
                    reply.error(libc::EISDIR);
                    return;
                }
            }
        }

        let api_path = full_path.trim_start_matches('/');
        match self
            .runtime
            .block_on(api::delete_file(&self.server_addr, api_path))
        {
            Ok(_) => {
                self.remove_cached_path(&full_path);
                reply.ok();
            }
            Err(err) => {
                error!("Failed to delete file {} on server: {:?}", api_path, err);
                reply.error(libc::EIO);
            }
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        // Resolve the directory path, ask the server to delete it, then clear cached children.
        let full_path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(err) => {
                reply.error(err);
                return;
            }
        };

        debug!("rmdir(parent={}, path='{}')", parent, full_path);

        // `rmdir` must reject regular files; those should go through `unlink`.
        if let Some(ino) = self.path_to_inode.lock().unwrap().get(&full_path).copied() {
            if let Some(attr) = self.inode_map.lock().unwrap().get(&ino) {
                if attr.attr.kind != FileType::Directory {
                    reply.error(libc::ENOTDIR);
                    return;
                }
            }
        }

        let api_path = full_path.trim_start_matches('/');
        match self
            .runtime
            .block_on(api::delete_file(&self.server_addr, api_path))
        {
            Ok(_) => {
                self.remove_cached_path(&full_path);
                reply.ok();
            }
            Err(err) => {
                error!(
                    "Failed to delete directory {} on server: {:?}",
                    api_path, err
                );
                reply.error(libc::EIO);
            }
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        // This implementation only handles normal rename semantics.
        if flags != 0 {
            reply.error(libc::EINVAL);
            return;
        }

        // Resolve both paths, send the move to the server, then update cached paths locally.
        let from_path = match self.resolve_rename_source_path(parent, name, newname) {
            Ok(path) => path,
            Err(err) => {
                reply.error(err);
                return;
            }
        };
        let to_path = match self.child_path(newparent, newname) {
            Ok(path) => path,
            Err(err) => {
                reply.error(err);
                return;
            }
        };

        debug!(
            "rename(parent={}, raw_name={:?}, resolved_name='{}', newparent={}, raw_newname={:?}, resolved_newname='{}', flags={})",
            parent, name, from_path, newparent, newname, to_path, flags
        );

        let api_from = from_path.trim_start_matches('/');
        let api_to = to_path.trim_start_matches('/');

        match self
            .runtime
            .block_on(api::rename_file(&self.server_addr, api_from, api_to))
        {
            Ok(_) => {
                self.rename_cached_path(&from_path, &to_path);
                reply.ok();
            }
            Err(err) => {
                error!(
                    "Failed to rename {} to {} on server: {:?}",
                    api_from, api_to, err
                );
                reply.error(libc::EIO);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!("readdir(ino={}, offset={})", ino, offset);

        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }

        let current_path = if _fh != 0 {
            match self.handle_for(_fh, ino, HandleKind::Directory) {
                Ok(handle) => handle.path,
                Err(err) => {
                    reply.error(err);
                    return;
                }
            }
        } else {
            match self.path_for_inode(ino) {
                Some(path) => path,
                None => {
                    error!(
                        "readdir: Could not find path for ino {}. Replying ENOENT.",
                        ino
                    );
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let mut entries_for_reply: Vec<(u64, FileType, String)> = Vec::new();

        // Standard entries: . and ..
        entries_for_reply.push((ino, FileType::Directory, ".".to_string()));
        // Determine parent inode for ".."
        let parent_ino_for_dotdot = if current_path == "/" {
            FUSE_ROOT_ID // Parent of root is root
        } else {
            // Find parent path, then its inode.
            let p = Path::new(&current_path);
            let parent_os_str = p.parent().unwrap_or_else(|| Path::new("/")).as_os_str();
            let parent_path_str = parent_os_str.to_str().unwrap_or("/").to_string();
            let path_map = self.path_to_inode.lock().unwrap();
            *path_map.get(&parent_path_str).unwrap_or(&FUSE_ROOT_ID)
        };
        entries_for_reply.push((parent_ino_for_dotdot, FileType::Directory, "..".to_string()));

        if offset < entries_for_reply.len() as i64 {
            match self.list_directory_cached(&current_path) {
                Ok(api_entries) => {
                    debug!(
                        "Successfully listed '{}' from server. Found {} entries.",
                        current_path,
                        api_entries.len()
                    );

                    for api_entry in api_entries {
                        let file_name = api_entry.name.clone();
                        let full_entry_path = if current_path == "/" {
                            format!("/{}", file_name)
                        } else {
                            format!("{}/{}", current_path.trim_end_matches('/'), file_name)
                        };

                        let attr = self.attr_from_entry_for_path(&full_entry_path, &api_entry);
                        self.cache_attr(full_entry_path.clone(), attr);
                        debug!(
                            "Added or refreshed entry: ino={}, path='{}', type={:?}",
                            attr.ino, full_entry_path, attr.kind
                        );
                        entries_for_reply.push((attr.ino, attr.kind, file_name));
                    }
                }
                Err(err) => {
                    error!(
                        "Failed to list directory {} from server: {:?}",
                        current_path, err
                    );
                    reply.error(ENOENT);
                    return;
                }
            }
        }

        for (i, entry) in entries_for_reply.iter().enumerate().skip(offset as usize) {
            let (entry_ino, entry_ftype, ref entry_name) = *entry;
            let entry_offset = (i + 1) as i64;
            debug!(
                "Adding to reply: ino={}, offset={}, type={:?}, name='{}'",
                entry_ino, entry_offset, entry_ftype, entry_name
            );
            if reply.add(entry_ino, entry_offset, entry_ftype, entry_name) {
                debug!("Reply buffer full after adding ino {}.", entry_ino);
                break;
            }
        }
        reply.ok();
        debug!("readdir for ino {} ({}) completed.", ino, current_path);
    }
}
