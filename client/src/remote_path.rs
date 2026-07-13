//! Pure helpers for the protocol's slash-separated, root-relative paths.

/// Converts an internal absolute path to the relative path expected by the API.
pub(crate) fn api(path: &str) -> &str {
    path.trim_start_matches('/')
}

/// Returns the parent of an internal absolute path.
pub(crate) fn parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &trimmed[..index],
    }
}

/// Joins a child name to an internal absolute directory path.
pub(crate) fn child(parent: &str, name: &str) -> String {
    if parent == "/" || parent.is_empty() {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_preserve_hierarchy_and_unicode() {
        assert_eq!(api("/directory/file.txt"), "directory/file.txt");
        assert_eq!(api("/"), "");
        assert_eq!(parent("/directory/file.txt"), "/directory");
        assert_eq!(parent("/file.txt"), "/");
        assert_eq!(child("/directory", "caffè.txt"), "/directory/caffè.txt");
        assert_eq!(child("/", "file.txt"), "/file.txt");
    }
}
