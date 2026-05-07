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
use std::time::{Duration, SystemTime};

use crate::api; // Assuming api.rs is in src/api.rs

const TTL: Duration = Duration::from_secs(1); // 1 second

// Helper function to create FileAttr
fn create_file_attr(ino: u64, kind: FileType, size: u64, perm: u16) -> FileAttr {
    FileAttr {
        ino,
        size,
        blocks: (size + 511) / 512, // Calculate blocks assuming 512 byte block size
        atime: SystemTime::now(),
        mtime: SystemTime::now(),
        ctime: SystemTime::now(),
        crtime: SystemTime::now(),
        kind,
        perm,
        nlink: if kind == FileType::Directory { 2 } else { 1 }, // Directories usually have nlink 2 (for . and ..)
        uid: 501, // Default UID; consider making this configurable or fetching actual
        gid: 20,  // Default GID; consider making this configurable or fetching actual
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

#[derive(Debug)]
pub struct RemoteFs {
    server_addr: String,
    runtime: Arc<tokio::runtime::Runtime>,
    inode_map: Arc<Mutex<HashMap<u64, FileAttr>>>,
    path_to_inode: Arc<Mutex<HashMap<String, u64>>>,
    next_inode: Arc<Mutex<u64>>,
}

impl RemoteFs {
    pub fn new(server_addr: &str) -> Self {
        let rt = Arc::new(tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime"));
        let mut inode_map_val = HashMap::new();
        let mut path_to_inode_val = HashMap::new();

        // Add root directory
        let root_attr = create_file_attr(FUSE_ROOT_ID, FileType::Directory, 0, 0o755);
        inode_map_val.insert(FUSE_ROOT_ID, root_attr);
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

        // 1. Find the parent directory's path
        let parent_path_str = {
            let path_to_inode_map = self.path_to_inode.lock().unwrap();
            path_to_inode_map
                .iter()
                .find_map(|(path, &inode)| {
                    if inode == parent {
                        Some(path.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "/".to_string())
        };

        // 2. Construct the full path of the NEW directory
        let full_path = if parent_path_str == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path_str.trim_end_matches('/'), name_str)
        };

        let api_path = full_path.trim_start_matches('/');

        // 3. Tell the remote server to create it
        match self
            .runtime
            .block_on(api::create_directory(&self.server_addr, api_path))
        {
            Ok(_) => {
                // 4. Success! Generate a new inode and cache it locally
                let mut inode_map_locked = self.inode_map.lock().unwrap();
                let mut path_to_inode_locked = self.path_to_inode.lock().unwrap();
                let mut next_inode_locked = self.next_inode.lock().unwrap();

                let new_ino = *next_inode_locked;
                *next_inode_locked += 1;

                // Create attributes for the new directory.
                // We use the mode provided by the OS, or fallback to 0o755.
                let attr = create_file_attr(new_ino, FileType::Directory, 0, mode as u16);

                inode_map_locked.insert(new_ino, attr);
                path_to_inode_locked.insert(full_path.clone(), new_ino);

                debug!(
                    "Successfully created and cached new directory: ino={}, path='{}'",
                    new_ino, full_path
                );

                // Tell the OS we succeeded and hand it the new attributes
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
        debug!("open(ino={})", ino);

        // Verify the inode exists and is a file
        let inode_map = self.inode_map.lock().unwrap();
        if let Some(attr) = inode_map.get(&ino) {
            if attr.kind == FileType::RegularFile {
                // Success! 0 for file handle (we aren't tracking handles yet), 0 for flags
                reply.opened(0, 0);
            } else {
                // It's a directory or something else
                reply.error(libc::EISDIR);
            }
        } else {
            reply.error(ENOENT);
        }
    }

    fn destroy(&mut self) {
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

        // 2. Check the Local Cache First
        {
            let path_to_inode_map = self.path_to_inode.lock().unwrap();
            let inode_map = self.inode_map.lock().unwrap();

            if let Some(&ino) = path_to_inode_map.get(&full_path) {
                if let Some(attr) = inode_map.get(&ino) {
                    debug!("CACHE HIT: Found attr for path {}: {:?}", full_path, attr);
                    reply.entry(&TTL, attr, 0);
                    return; // We are done!
                }
            }
        } // The Mutex locks are safely dropped here!

        // 3. Cache Miss: Ask the server about the parent directory
        debug!(
            "CACHE MISS: Fetching parent directory '{}' from server...",
            parent_path_str
        );

        let api_path = if parent_path_str == "/" {
            "".to_string()
        } else {
            parent_path_str.trim_start_matches('/').to_string()
        };

        match self
            .runtime
            .block_on(api::list_directory(&self.server_addr, &api_path))
        {
            Ok(api_entries) => {
                // Look for the specific file the OS asked for in the server's response
                if let Some(api_entry) = api_entries.into_iter().find(|e| e.name == name_str) {
                    // We found it! Generate a new inode and cache it.
                    let mut inode_map_locked = self.inode_map.lock().unwrap();
                    let mut path_to_inode_locked = self.path_to_inode.lock().unwrap();
                    let mut next_inode_locked = self.next_inode.lock().unwrap();

                    let new_ino = *next_inode_locked;
                    *next_inode_locked += 1;

                    let kind = if api_entry.type_ == "directory" {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    let perm = if kind == FileType::Directory {
                        0o755
                    } else {
                        0o644
                    };

                    // Reusing your helper function
                    let attr = create_file_attr(new_ino, kind, api_entry.size, perm);

                    inode_map_locked.insert(new_ino, attr);
                    path_to_inode_locked.insert(full_path.clone(), new_ino);

                    debug!(
                        "Dynamically added new entry to cache: ino={}, path='{}'",
                        new_ino, full_path
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
        let inode_map = self.inode_map.lock().unwrap();
        match inode_map.get(&ino) {
            Some(attr) => {
                reply.attr(&TTL, attr);
            }
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

        // 1. Find the parent directory's path
        let parent_path_str = {
            let path_to_inode_map = self.path_to_inode.lock().unwrap();
            path_to_inode_map
                .iter()
                .find_map(|(path, &inode)| {
                    if inode == parent {
                        Some(path.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "/".to_string())
        };

        // 2. Construct the full path
        let full_path = if parent_path_str == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path_str.trim_end_matches('/'), name_str)
        };
        let api_path = full_path.trim_start_matches('/');

        // 3. Tell the remote server to initialize the empty file
        match self
            .runtime
            .block_on(api::create_file(&self.server_addr, api_path))
        {
            Ok(_) => {
                // 4. Update the local cache with the new inode
                let mut inode_map_locked = self.inode_map.lock().unwrap();
                let mut path_to_inode_locked = self.path_to_inode.lock().unwrap();
                let mut next_inode_locked = self.next_inode.lock().unwrap();

                let new_ino = *next_inode_locked;
                *next_inode_locked += 1;

                let attr = create_file_attr(new_ino, FileType::RegularFile, 0, mode as u16);

                inode_map_locked.insert(new_ino, attr);
                path_to_inode_locked.insert(full_path.clone(), new_ino);

                debug!(
                    "Successfully created file: ino={}, path='{}'",
                    new_ino, full_path
                );

                // Reply with the new attributes, generation 0, file handle 0, and no special flags
                reply.created(&TTL, &attr, 0, 0, 0);
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

        // 1. Find the file's path using the inode
        let path = {
            let path_map = self.path_to_inode.lock().unwrap();
            path_map
                .iter()
                .find_map(|(p, &i)| if i == ino { Some(p.clone()) } else { None })
        };

        let file_path = match path {
            Some(p) => p,
            None => {
                error!("write: Could not find path for ino {}", ino);
                reply.error(ENOENT);
                return;
            }
        };
        let api_path = file_path.trim_start_matches('/');

        // 2. Send the chunk of bytes to the server
        match self.runtime.block_on(api::write_file(
            &self.server_addr,
            api_path,
            data,
            offset as u64,
        )) {
            Ok(_) => {
                // 3. Update the file size in our local cache so `ls` shows the correct size
                let mut inode_map = self.inode_map.lock().unwrap();
                if let Some(attr) = inode_map.get_mut(&ino) {
                    let new_size = std::cmp::max(attr.size, (offset as u64) + (data.len() as u64));
                    attr.size = new_size;
                    attr.blocks = (new_size + 511) / 512;
                    attr.mtime = SystemTime::now(); // Update modified time
                }

                // 4. Tell the OS exactly how many bytes we successfully wrote
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

        // 1. Resolve path (needed in case we are truncating the file to 0 bytes)
        let path = {
            let path_map = self.path_to_inode.lock().unwrap();
            path_map
                .iter()
                .find_map(|(p, &i)| if i == ino { Some(p.clone()) } else { None })
        };

        // If the OS resizes a file, mirror that size change on the server.
        if let Some(s) = size {
            if let Some(ref p) = path {
                let api_path = p.trim_start_matches('/');
                if let Err(err) =
                    self.runtime
                        .block_on(api::resize_file(&self.server_addr, api_path, s))
                {
                    error!("Failed to resize file {} on server: {:?}", api_path, err);
                    reply.error(libc::EIO);
                    return;
                }
            }
        }

        // 2. Update local cache attributes
        let mut inode_map = self.inode_map.lock().unwrap();
        if let Some(attr) = inode_map.get_mut(&ino) {
            if let Some(s) = size {
                attr.size = s;
                attr.blocks = (s + 511) / 512;
            }
            if let Some(m) = mode {
                attr.perm = m as u16;
            }
            if let Some(u) = uid {
                attr.uid = u;
            }
            if let Some(g) = gid {
                attr.gid = g;
            }

            attr.mtime = SystemTime::now();

            reply.attr(&TTL, attr);
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

        // 1. Find the path associated with this inode
        let path = {
            let path_map = self.path_to_inode.lock().unwrap();
            path_map
                .iter()
                .find_map(|(p, &i)| if i == ino { Some(p.clone()) } else { None })
        };
        let file_path = match path {
            Some(p) => p,
            None => {
                error!("read: Could not find path for ino {}", ino);
                reply.error(ENOENT);
                return;
            }
        };
        // Clean up the path for the API (remove leading slash)
        let api_path = file_path.trim_start_matches('/');

        // 2. Fetch only the byte range requested by the kernel.
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
                if attr.kind == FileType::Directory {
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
                if attr.kind != FileType::Directory {
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

        // For this simplified version, we only allow readdir on the root inode.
        // A more complete solution would map `ino` to a path and fetch that path.
        let current_path = if ino == FUSE_ROOT_ID {
            "/".to_string()
        } else {
            // Try to find path by ino for non-root.
            // This is a simple reverse lookup; a more robust system might be needed.
            let path_map = self.path_to_inode.lock().unwrap();
            if let Some(p) = path_map
                .iter()
                .find_map(|(p_str, &i)| if i == ino { Some(p_str.clone()) } else { None })
            {
                p
            } else {
                error!(
                    "readdir: Could not find path for ino {}. Replying ENOENT.",
                    ino
                );
                reply.error(ENOENT);
                return;
            }
        };
        // The API expects "" for root, and "path/" for subdirectories.
        let api_path = if current_path == "/" {
            "".to_string()
        } else {
            current_path.trim_start_matches('/').to_string()
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
            // Only fetch from server if we haven't passed ., ..
            match self
                .runtime
                .block_on(api::list_directory(&self.server_addr, &api_path))
            {
                Ok(api_entries) => {
                    debug!(
                        "Successfully listed '{}' from server. Found {} entries.",
                        api_path,
                        api_entries.len()
                    );
                    let mut inode_map_locked = self.inode_map.lock().unwrap();
                    let mut path_to_inode_locked = self.path_to_inode.lock().unwrap();
                    let mut next_inode_locked = self.next_inode.lock().unwrap();

                    for api_entry in api_entries {
                        let file_name = api_entry.name;
                        // Construct full path relative to the current directory being listed
                        let full_entry_path = if current_path == "/" {
                            format!("/{}", file_name)
                        } else {
                            format!("{}/{}", current_path.trim_end_matches('/'), file_name)
                        };

                        let (entry_ino, entry_kind) = if let Some(&existing_ino) =
                            path_to_inode_locked.get(&full_entry_path)
                        {
                            let attr = inode_map_locked.get_mut(&existing_ino).unwrap(); // Should exist
                            attr.size = api_entry.size;
                            (existing_ino, attr.kind)
                        } else {
                            let new_ino = *next_inode_locked;
                            *next_inode_locked += 1;
                            let kind = if api_entry.type_ == "directory" {
                                FileType::Directory
                            } else {
                                FileType::RegularFile
                            };
                            let perm = if kind == FileType::Directory {
                                0o755
                            } else {
                                0o644
                            };
                            let attr = create_file_attr(new_ino, kind, api_entry.size, perm);

                            inode_map_locked.insert(new_ino, attr);
                            path_to_inode_locked.insert(full_entry_path.clone(), new_ino);
                            debug!(
                                "Added new entry to maps: ino={}, path='{}', type={:?}",
                                new_ino, full_entry_path, kind
                            );
                            (new_ino, kind)
                        };
                        entries_for_reply.push((entry_ino, entry_kind, file_name));
                    }
                }
                Err(err) => {
                    error!(
                        "Failed to list directory {} from server: {:?}",
                        api_path, err
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
        debug!("readdir for ino {} ({}) completed.", ino, api_path);
    }
}
