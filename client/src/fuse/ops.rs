use super::{
    HandleKind, RemoteFs, TTL, api, apply_umask, attr_from_remote_metadata, errno_from_api_error,
    time_or_now,
};
use fuser::{
    FUSE_ROOT_ID, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, Request,
};
use libc::{ENOENT, c_int};
use log::{debug, error, info, warn};
use std::ffi::OsStr;
use std::path::Path;
use std::time::SystemTime;

// FUSE callbacks translate kernel operations into HTTP API calls and keep
// RemoteFs' local inode/path caches coherent after each successful mutation.
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
                reply.error(errno_from_api_error(&err));
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
                error!("Failed to lookup {} on server: {:?}", full_path, err);
                reply.error(errno_from_api_error(&err));
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
                reply.error(errno_from_api_error(&err));
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
                reply.error(errno_from_api_error(&err));
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
                    reply.error(errno_from_api_error(&err));
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
                    reply.error(errno_from_api_error(&err));
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
                reply.error(errno_from_api_error(&err));
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
                reply.error(errno_from_api_error(&err));
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
                reply.error(errno_from_api_error(&err));
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
                reply.error(errno_from_api_error(&err));
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
                    reply.error(errno_from_api_error(&err));
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
