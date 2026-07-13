use crate::error::StorageError;
use axum::http::HeaderMap;
use remote_fs_protocol::{headers, DirectoryEntry, RemoteMetadata};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::fs;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, PermissionsExt},
};

#[cfg(not(unix))]
const DEFAULT_FILE_MODE: u32 = 0o644;
#[cfg(not(unix))]
const DEFAULT_DIR_MODE: u32 = 0o755;

fn format_modified_at(modified_at: SystemTime) -> String {
    modified_at
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(unix)]
fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    if metadata.is_dir() {
        DEFAULT_DIR_MODE
    } else if metadata.permissions().readonly() {
        0o444
    } else {
        DEFAULT_FILE_MODE
    }
}

#[cfg(unix)]
fn metadata_uid(metadata: &std::fs::Metadata) -> Option<u32> {
    Some(metadata.uid())
}

#[cfg(not(unix))]
fn metadata_uid(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn metadata_gid(metadata: &std::fs::Metadata) -> Option<u32> {
    Some(metadata.gid())
}

#[cfg(not(unix))]
fn metadata_gid(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

fn entry_type(metadata: &std::fs::Metadata) -> Option<String> {
    if metadata.is_dir() {
        Some("directory".to_string())
    } else if metadata.is_file() {
        Some("file".to_string())
    } else {
        None
    }
}

pub(crate) fn entry_metadata_from_metadata(metadata: std::fs::Metadata) -> Option<RemoteMetadata> {
    let type_ = entry_type(&metadata)?;

    Some(RemoteMetadata {
        type_,
        size: if metadata.is_file() {
            metadata.len()
        } else {
            0
        },
        modified_at: metadata
            .modified()
            .map(format_modified_at)
            .unwrap_or_else(|_| "0".to_string()),
        mode: Some(metadata_mode(&metadata)),
        uid: metadata_uid(&metadata),
        gid: metadata_gid(&metadata),
    })
}

pub(crate) async fn entry_metadata_for_path(path: &Path) -> Result<RemoteMetadata, StorageError> {
    let metadata = fs::metadata(path)
        .await
        .map_err(|error| StorageError::from_io(error, "Path not found"))?;

    entry_metadata_from_metadata(metadata).ok_or(StorageError::BadRequest("Unsupported file type"))
}

pub(crate) fn directory_entry_from_metadata(
    name: String,
    metadata: std::fs::Metadata,
) -> Option<DirectoryEntry> {
    let entry = entry_metadata_from_metadata(metadata)?;

    Some(DirectoryEntry {
        name,
        type_: entry.type_,
        size: entry.size,
        modified_at: entry.modified_at,
        mode: entry.mode,
        uid: entry.uid,
        gid: entry.gid,
    })
}

pub(crate) fn parse_optional_u64_header(
    headers_map: &HeaderMap,
    name: &'static str,
) -> Result<Option<u64>, StorageError> {
    headers_map
        .get(name)
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|text| text.parse::<u64>().ok())
                .ok_or(StorageError::BadRequest("Invalid numeric header"))
        })
        .transpose()
}

fn parse_optional_u32_header(
    headers_map: &HeaderMap,
    name: &'static str,
) -> Result<Option<u32>, StorageError> {
    headers_map
        .get(name)
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|text| text.parse::<u32>().ok())
                .ok_or(StorageError::BadRequest("Invalid numeric header"))
        })
        .transpose()
}

fn parse_optional_mode_header(headers_map: &HeaderMap) -> Result<Option<u32>, StorageError> {
    headers_map
        .get(headers::FILE_MODE)
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|text| u32::from_str_radix(text, 8).ok())
                .filter(|mode| *mode <= 0o7777)
                .ok_or(StorageError::BadRequest("Invalid file mode"))
        })
        .transpose()
}

#[cfg(unix)]
fn path_to_cstring(path: &Path) -> Result<CString, io::Error> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))
}

async fn apply_mode(path: &Path, mode: u32) -> Result<(), StorageError> {
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)
            .await
            .map_err(|error| StorageError::from_io(error, "Path not found"))?
            .permissions();
        permissions.set_mode(mode & 0o7777);
        fs::set_permissions(path, permissions)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not update permissions"))?;
    }

    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)
            .await
            .map_err(|error| StorageError::from_io(error, "Path not found"))?
            .permissions();
        permissions.set_readonly(mode & 0o222 == 0);
        fs::set_permissions(path, permissions)
            .await
            .map_err(|error| StorageError::from_io(error, "Could not update permissions"))?;
    }

    Ok(())
}

fn apply_owner(path: &Path, uid: Option<u32>, gid: Option<u32>) -> Result<(), StorageError> {
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }

    #[cfg(not(unix))]
    let _ = (path, uid, gid);

    #[cfg(unix)]
    {
        let c_path =
            path_to_cstring(path).map_err(|error| StorageError::from_io(error, "Invalid path"))?;
        let uid = uid
            .map(|value| value as libc::uid_t)
            .unwrap_or(!0 as libc::uid_t);
        let gid = gid
            .map(|value| value as libc::gid_t)
            .unwrap_or(!0 as libc::gid_t);

        let result = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
        if result != 0 {
            return Err(StorageError::from_io(
                io::Error::last_os_error(),
                "Could not update owner",
            ));
        }
    }

    Ok(())
}

#[cfg(unix)]
fn timespec_from_system_time(time: SystemTime) -> libc::timespec {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));

    libc::timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}

fn apply_modified_time(path: &Path, modified_at: u64) -> Result<(), StorageError> {
    #[cfg(windows)]
    {
        use std::fs::{FileTimes, OpenOptions as StdOpenOptions};
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

        let requested_time = UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(modified_at))
            .ok_or(StorageError::BadRequest(
                "Modification time is out of range",
            ))?;
        let file = StdOpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(|error| StorageError::from_io(error, "Could not open path"))?;

        file.set_times(FileTimes::new().set_modified(requested_time))
            .map_err(|error| StorageError::from_io(error, "Could not update modification time"))?;
    }

    #[cfg(all(not(unix), not(windows)))]
    let _ = (path, modified_at);

    #[cfg(unix)]
    {
        let c_path =
            path_to_cstring(path).map_err(|error| StorageError::from_io(error, "Invalid path"))?;
        let metadata = std::fs::metadata(path)
            .map_err(|error| StorageError::from_io(error, "Path not found"))?;
        let accessed_at = metadata.accessed().unwrap_or(UNIX_EPOCH);
        let times = [
            timespec_from_system_time(accessed_at),
            libc::timespec {
                tv_sec: modified_at as libc::time_t,
                tv_nsec: 0,
            },
        ];

        let result = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
        if result != 0 {
            return Err(StorageError::from_io(
                io::Error::last_os_error(),
                "Could not update modification time",
            ));
        }
    }

    Ok(())
}

pub(crate) async fn apply_metadata_headers(
    path: &Path,
    headers_map: &HeaderMap,
) -> Result<(), StorageError> {
    if let Some(mode) = parse_optional_mode_header(headers_map)? {
        apply_mode(path, mode).await?;
    }

    let uid = parse_optional_u32_header(headers_map, headers::FILE_UID)?;
    let gid = parse_optional_u32_header(headers_map, headers::FILE_GID)?;
    apply_owner(path, uid, gid)?;

    if let Some(modified_at) = parse_optional_u64_header(headers_map, headers::FILE_MTIME)? {
        apply_modified_time(path, modified_at)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_numeric_and_mode_headers() {
        let mut headers_map = HeaderMap::new();
        headers_map.insert(headers::FILE_OFFSET, "not-a-number".parse().unwrap());
        assert!(parse_optional_u64_header(&headers_map, headers::FILE_OFFSET).is_err());

        headers_map.clear();
        headers_map.insert(headers::FILE_MODE, "10000".parse().unwrap());
        assert!(parse_optional_mode_header(&headers_map).is_err());
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_metadata_marks_uid_and_gid_as_unsupported() {
        let path = std::env::temp_dir().join(format!(
            "remote-fs-metadata-owner-test-{}",
            std::process::id()
        ));
        std::fs::write(&path, b"owner test").unwrap();
        let metadata = entry_metadata_from_metadata(std::fs::metadata(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(metadata.uid, None);
        assert_eq!(metadata.gid, None);
    }
}
