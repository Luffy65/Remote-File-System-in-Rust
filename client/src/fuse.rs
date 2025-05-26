use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
    FUSE_ROOT_ID,
};
use libc::{c_int, ENOENT};
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

        info!("RemoteFs initialized. Root inode: {}, Path: /", FUSE_ROOT_ID);

        RemoteFs {
            server_addr: server_addr.to_string(),
            runtime: rt,
            inode_map: Arc::new(Mutex::new(inode_map_val)),
            path_to_inode: Arc::new(Mutex::new(path_to_inode_val)),
            next_inode: Arc::new(Mutex::new(FUSE_ROOT_ID + 1)),
        }
    }
}

impl Filesystem for RemoteFs {
    fn init(&mut self, _req: &Request<'_>, _config: &mut fuser::KernelConfig) -> Result<(), c_int> {
        info!("Filesystem init method called.");
        Ok(())
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

        let path_to_inode_map = self.path_to_inode.lock().unwrap();
        let inode_map = self.inode_map.lock().unwrap();

        let parent_path_str = path_to_inode_map.iter()
            .find_map(|(path, &inode)| if inode == parent_ino { Some(path.clone()) } else { None })
            .unwrap_or_else(|| {
                warn!("Parent path for ino {} not found, defaulting to root for lookup.", parent_ino);
                "/".to_string()
            });

        let full_path = if parent_path_str == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path_str.trim_end_matches('/'), name_str)
        };
        
        debug!("Looking up full path: {}", full_path);

        if let Some(&ino) = path_to_inode_map.get(&full_path) {
            if let Some(attr) = inode_map.get(&ino) {
                debug!("Found attr for path {}: {:?}", full_path, attr);
                reply.entry(&TTL, attr, 0); // 0 generation
            } else {
                error!("Inode {} for path {} found in path_to_inode but not in inode_map!", ino, full_path);
                reply.error(ENOENT);
            }
        } else {
            debug!("Path {} not found in path_to_inode map.", full_path);
            reply.error(ENOENT);
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

    fn read(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _offset: i64,
        _size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        debug!("read called for ino {}. Currently not implemented beyond dummy files.", _ino);
        reply.error(ENOENT);
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
            if let Some(p) = path_map.iter().find_map(|(p_str, &i)| if i == ino { Some(p_str.clone()) } else { None }) {
                p
            } else {
                error!("readdir: Could not find path for ino {}. Replying ENOENT.", ino);
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
        
        if offset < entries_for_reply.len() as i64 { // Only fetch from server if we haven't passed ., ..
            match self.runtime.block_on(api::list_directory(&self.server_addr, &api_path)) {
                Ok(api_entries) => {
                    debug!("Successfully listed '{}' from server. Found {} entries.", api_path, api_entries.len());
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

                        let (entry_ino, entry_kind) = if let Some(&existing_ino) = path_to_inode_locked.get(&full_entry_path) {
                            let attr = inode_map_locked.get_mut(&existing_ino).unwrap(); // Should exist
                            attr.size = api_entry.size;
                            (existing_ino, attr.kind)
                        } else {
                            let new_ino = *next_inode_locked;
                            *next_inode_locked += 1;
                            let kind = if api_entry.type_ == "directory" { FileType::Directory } else { FileType::RegularFile };
                            let perm = if kind == FileType::Directory { 0o755 } else { 0o644 };
                            let attr = create_file_attr(new_ino, kind, api_entry.size, perm);
                            
                            inode_map_locked.insert(new_ino, attr);
                            path_to_inode_locked.insert(full_entry_path.clone(), new_ino);
                            debug!("Added new entry to maps: ino={}, path='{}', type={:?}", new_ino, full_entry_path, kind);
                            (new_ino, kind)
                        };
                        entries_for_reply.push((entry_ino, entry_kind, file_name));
                    }
                }
                Err(err) => {
                    error!("Failed to list directory {} from server: {:?}", api_path, err);
                    reply.error(ENOENT);
                    return;
                }
            }
        }
        
        for (i, entry) in entries_for_reply.iter().enumerate().skip(offset as usize) {
            let (entry_ino, entry_ftype, ref entry_name) = *entry;
            let entry_offset = (i + 1) as i64; 
            debug!("Adding to reply: ino={}, offset={}, type={:?}, name='{}'", entry_ino, entry_offset, entry_ftype, entry_name);
            if reply.add(entry_ino, entry_offset, entry_ftype, entry_name) {
                debug!("Reply buffer full after adding ino {}.", entry_ino);
                break; 
            }
        }
        reply.ok();
        debug!("readdir for ino {} ({}) completed.", ino, api_path);
    }
}
