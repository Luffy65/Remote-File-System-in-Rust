use fuser::{FUSE_ROOT_ID, FileAttr, FileType};
use libc::{ENOENT, c_int};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api;
use crate::cache::TtlLruCache;

// Keep the long fuser::Filesystem callback implementation separate from
// the state/cache helpers in this file.
mod ops;

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

fn errno_from_api_error(error: &reqwest::Error) -> c_int {
    match error.status().map(|status| status.as_u16()) {
        Some(400) => libc::EINVAL,
        Some(401 | 403) => libc::EACCES,
        Some(404) => ENOENT,
        Some(409) => libc::EEXIST,
        _ => libc::EIO,
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
    dirty: bool,
}

// Client-side state that gives stateless HTTP paths stable FUSE inodes,
// open file handles, and short-lived directory/attribute caches.
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

    fn allocate_handle(&self, ino: u64, path: String, kind: HandleKind, _flags: i32) -> u64 {
        let mut next_handle = self.next_handle.lock().unwrap();
        let handle = *next_handle;
        *next_handle += 1;

        self.open_handles.lock().unwrap().insert(
            handle,
            OpenHandle {
                ino,
                path,
                kind,
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
        // Directory listings are cached briefly because kernels can call
        // lookup/getattr/readdir in tight bursts for the same parent.
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
