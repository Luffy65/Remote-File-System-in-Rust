use crate::{error::StorageError, AppState, INTERNAL_DIR_NAME};
use std::{
    io,
    path::{Component, Path, PathBuf},
};

impl AppState {
    /// Resolve an API path below the storage root without following a link or
    /// reparse-point component outside that root.
    pub(crate) fn resolve_path(&self, path: &str) -> Result<PathBuf, StorageError> {
        let relative_path = sanitize_api_path(path)?;
        reject_internal_path(&relative_path)?;
        self.reject_link_components(&relative_path)?;
        Ok(self.root_dir.join(relative_path))
    }

    /// Resolve a mutable API path and reject the storage root itself.
    pub(crate) fn resolve_non_root_path(&self, path: &str) -> Result<PathBuf, StorageError> {
        let relative_path = sanitize_api_path(path)?;
        if relative_path.as_os_str().is_empty() {
            return Err(StorageError::BadRequest("Path cannot be empty"));
        }

        reject_internal_path(&relative_path)?;
        self.reject_link_components(&relative_path)?;
        Ok(self.root_dir.join(relative_path))
    }

    fn reject_link_components(&self, relative_path: &Path) -> Result<(), StorageError> {
        let mut current = self.root_dir.clone();

        for component in relative_path.components() {
            let Component::Normal(part) = component else {
                return Err(StorageError::BadRequest("Invalid path"));
            };
            current.push(part);

            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if is_link_or_reparse_point(&metadata) => {
                    return Err(StorageError::Forbidden(
                        "Symbolic links and reparse points are not allowed",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(StorageError::from_io(error, "Could not inspect path"));
                }
            }
        }

        Ok(())
    }
}

fn sanitize_api_path(path: &str) -> Result<PathBuf, StorageError> {
    let trimmed_path = path.trim_matches('/');
    let mut relative_path = PathBuf::new();

    if trimmed_path.is_empty() {
        return Ok(relative_path);
    }

    for component in Path::new(trimmed_path).components() {
        match component {
            Component::Normal(part) => relative_path.push(part),
            _ => return Err(StorageError::BadRequest("Invalid path")),
        }
    }

    Ok(relative_path)
}

fn reject_internal_path(path: &Path) -> Result<(), StorageError> {
    if path
        .components()
        .next()
        .is_some_and(|component| component.as_os_str() == INTERNAL_DIR_NAME)
    {
        return Err(StorageError::Forbidden("Path is reserved by the server"));
    }
    Ok(())
}

#[cfg(unix)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(all(not(unix), not(windows)))]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::sanitize_api_path;

    #[test]
    fn path_sanitizer_accepts_normal_components_only() {
        assert_eq!(
            sanitize_api_path("/docs/report.txt").unwrap(),
            Path::new("docs/report.txt")
        );
        assert!(sanitize_api_path("../outside").is_err());
        assert!(sanitize_api_path("docs/../../outside").is_err());
    }

    use std::path::Path;
}
