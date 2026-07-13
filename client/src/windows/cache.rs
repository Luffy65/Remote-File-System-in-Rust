//! Short-lived Windows metadata and directory caches.
//!
//! Cache entries are only hints: every mutation updates or invalidates the
//! affected directory and metadata tree, and expired entries are never served.

use super::{DirectoryEntry, RemoteEntry, directory_entry_from_remote};
use crate::remote_path;
use std::{
    collections::HashMap,
    io::ErrorKind,
    sync::Mutex,
    time::{Duration, Instant},
};
use winfsp::{FspError, Result};

const METADATA_TTL: Duration = Duration::from_secs(1);
const METADATA_MAX_ENTRIES: usize = 4096;
const DIRECTORY_TTL: Duration = Duration::from_secs(1);
const DIRECTORY_MAX_ENTRIES: usize = 256;

#[derive(Clone, Debug)]
struct CachedMetadata {
    entry: RemoteEntry,
    cached_at: Instant,
}

#[derive(Clone, Debug)]
struct CachedDirectory {
    entries: Vec<DirectoryEntry>,
    cached_at: Instant,
}

pub(super) struct WindowsCache {
    metadata: Mutex<HashMap<String, CachedMetadata>>,
    directories: Mutex<HashMap<String, CachedDirectory>>,
}

impl WindowsCache {
    pub(super) fn new() -> Self {
        Self {
            metadata: Mutex::new(HashMap::new()),
            directories: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn metadata(&self, path: &str) -> Option<RemoteEntry> {
        let mut cache = self.metadata.lock().unwrap();
        if let Some(cached) = cache.get(path)
            && cached.cached_at.elapsed() <= METADATA_TTL
        {
            return Some(cached.entry.clone());
        }
        cache.remove(path);
        None
    }

    pub(super) fn parent_metadata(&self, path: &str) -> Option<Result<RemoteEntry>> {
        if path == "/" {
            return None;
        }

        let parent = remote_path::parent(path);
        let name = path.rsplit('/').next().unwrap_or_default();
        let mut cache = self.directories.lock().unwrap();
        let cached = cache.get(parent)?;
        if cached.cached_at.elapsed() > DIRECTORY_TTL {
            cache.remove(parent);
            return None;
        }

        Some(
            cached
                .entries
                .iter()
                .find(|entry| entry.name == name)
                .map(RemoteEntry::from_directory_entry)
                .ok_or(FspError::IO(ErrorKind::NotFound)),
        )
    }

    pub(super) fn insert_metadata(&self, path: &str, entry: &RemoteEntry) {
        let mut cache = self.metadata.lock().unwrap();
        prune_or_clear(&mut cache, METADATA_MAX_ENTRIES, METADATA_TTL, |entry| {
            entry.cached_at
        });
        cache.insert(
            path.to_string(),
            CachedMetadata {
                entry: entry.clone(),
                cached_at: Instant::now(),
            },
        );
    }

    pub(super) fn invalidate_metadata_tree(&self, path: &str) {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        self.metadata
            .lock()
            .unwrap()
            .retain(|cached_path, _| cached_path != path && !cached_path.starts_with(&prefix));
    }

    pub(super) fn directory(&self, path: &str) -> Option<Vec<DirectoryEntry>> {
        let mut cache = self.directories.lock().unwrap();
        if let Some(cached) = cache.get(path)
            && cached.cached_at.elapsed() <= DIRECTORY_TTL
        {
            return Some(cached.entries.clone());
        }
        cache.remove(path);
        None
    }

    pub(super) fn insert_directory(&self, path: &str, entries: Vec<DirectoryEntry>) {
        let mut cache = self.directories.lock().unwrap();
        prune_or_clear(&mut cache, DIRECTORY_MAX_ENTRIES, DIRECTORY_TTL, |entry| {
            entry.cached_at
        });
        cache.insert(
            path.to_string(),
            CachedDirectory {
                entries,
                cached_at: Instant::now(),
            },
        );
    }

    pub(super) fn update_directory_for_path(&self, path: &str, entry: &RemoteEntry) {
        if path == "/" {
            return;
        }
        let parent = remote_path::parent(path);
        let Some(name) = path.rsplit('/').next().filter(|name| !name.is_empty()) else {
            return;
        };
        let mut cache = self.directories.lock().unwrap();
        let Some(cached) = cache.get_mut(parent) else {
            return;
        };
        if cached.cached_at.elapsed() > DIRECTORY_TTL {
            cache.remove(parent);
            return;
        }
        cached.entries.retain(|item| item.name != name);
        cached
            .entries
            .push(directory_entry_from_remote(name, entry));
        cached
            .entries
            .sort_by(|left, right| left.name.cmp(&right.name));
        cached.cached_at = Instant::now();
    }

    pub(super) fn remove_path(&self, path: &str) {
        if path == "/" {
            self.directories.lock().unwrap().clear();
            return;
        }
        let parent = remote_path::parent(path);
        let name = path.rsplit('/').next().unwrap_or_default();
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut cache = self.directories.lock().unwrap();
        cache.retain(|cached_path, _| cached_path != path && !cached_path.starts_with(&prefix));
        if let Some(cached) = cache.get_mut(parent) {
            if cached.cached_at.elapsed() <= DIRECTORY_TTL {
                cached.entries.retain(|entry| entry.name != name);
                cached.cached_at = Instant::now();
            } else {
                cache.remove(parent);
            }
        }
    }

    pub(super) fn clear(&self) {
        self.metadata.lock().unwrap().clear();
        self.directories.lock().unwrap().clear();
    }
}

fn prune_or_clear<T>(
    cache: &mut HashMap<String, T>,
    max_entries: usize,
    ttl: Duration,
    cached_at: impl Fn(&T) -> Instant,
) {
    if cache.len() < max_entries {
        return;
    }
    cache.retain(|_, entry| cached_at(entry).elapsed() <= ttl);
    if cache.len() >= max_entries {
        cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::EntryKind;

    fn file() -> RemoteEntry {
        RemoteEntry {
            kind: EntryKind::File,
            size: 7,
            modified_at: "1".to_string(),
            mode: Some(0o644),
        }
    }

    #[test]
    fn mutation_updates_parent_listing_and_tree_invalidation_removes_metadata() {
        let cache = WindowsCache::new();
        cache.insert_directory("/parent", Vec::new());
        cache.update_directory_for_path("/parent/file", &file());
        assert_eq!(cache.directory("/parent").unwrap()[0].name, "file");

        cache.insert_metadata("/parent/file", &file());
        cache.invalidate_metadata_tree("/parent");
        assert!(cache.metadata("/parent/file").is_none());

        cache.remove_path("/parent/file");
        assert!(cache.directory("/parent").unwrap().is_empty());
    }
}
