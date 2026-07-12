use serde::{Deserialize, Serialize};
use std::{
    fmt::Write,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize, Debug, Clone)]
pub struct DirectoryEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub size: u64,
    pub modified_at: String,
    pub mode: Option<u32>,
    #[cfg(not(windows))]
    pub uid: Option<u32>,
    #[cfg(not(windows))]
    pub gid: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RemoteMetadata {
    #[serde(rename = "type")]
    pub type_: String,
    pub size: u64,
    pub modified_at: String,
    pub mode: Option<u32>,
    #[cfg(not(windows))]
    pub uid: Option<u32>,
    #[cfg(not(windows))]
    pub gid: Option<u32>,
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("failed to build HTTP client")
    })
}

fn authenticated(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match std::env::var("REMOTE_FS_TOKEN") {
        Ok(token) if !token.trim().is_empty() => request.bearer_auth(token.trim()),
        _ => request,
    }
}

fn encode_api_path(path: &str) -> String {
    let normalized = path.trim_matches('/');
    let mut encoded = String::with_capacity(normalized.len());

    for (component_index, component) in normalized.split('/').enumerate() {
        if component_index > 0 {
            encoded.push('/');
        }

        for byte in component.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                encoded.push(char::from(byte));
            } else {
                write!(&mut encoded, "%{byte:02X}").expect("writing to a String cannot fail");
            }
        }
    }

    encoded
}

fn endpoint_url(base_url: &str, endpoint: &str, path: &str) -> String {
    format!(
        "{}/{}/{}",
        base_url.trim_end_matches('/'),
        endpoint,
        encode_api_path(path)
    )
}

fn unix_seconds_from_system_time(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

fn add_optional_metadata_headers(
    mut request: reqwest::RequestBuilder,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    modified_at: Option<SystemTime>,
) -> reqwest::RequestBuilder {
    if let Some(mode) = mode {
        request = request.header("X-File-Mode", format!("{:o}", mode & 0o7777));
    }
    if let Some(uid) = uid {
        request = request.header("X-File-Uid", uid.to_string());
    }
    if let Some(gid) = gid {
        request = request.header("X-File-Gid", gid.to_string());
    }
    if let Some(modified_at) = modified_at {
        request = request.header(
            "X-File-Mtime",
            unix_seconds_from_system_time(modified_at).to_string(),
        );
    }

    request
}

pub async fn list_directory(
    base_url: &str,
    path: &str,
) -> Result<Vec<DirectoryEntry>, reqwest::Error> {
    let request_url = endpoint_url(base_url, "list", path);

    log::debug!("Requesting directory list from URL: {}", request_url);

    let response = authenticated(http_client().get(&request_url))
        .send()
        .await?;
    log::debug!("Received response: {:?}", response.status());

    let entries = response
        .error_for_status()?
        .json::<Vec<DirectoryEntry>>()
        .await?;
    Ok(entries)
}

pub async fn get_metadata(base_url: &str, path: &str) -> Result<RemoteMetadata, reqwest::Error> {
    let request_url = endpoint_url(base_url, "metadata", path);

    log::debug!("Requesting metadata from URL: {}", request_url);

    authenticated(http_client().get(&request_url))
        .send()
        .await?
        .error_for_status()?
        .json::<RemoteMetadata>()
        .await
}

// Requests only one byte range from the server instead of downloading the whole file.
pub async fn read_file(
    base_url: &str,
    path: &str,
    offset: u64,
    size: u32,
) -> Result<Vec<u8>, reqwest::Error> {
    let request_url = endpoint_url(base_url, "files", path);

    log::debug!(
        "Requesting file range from URL: {} (offset={}, size={})",
        request_url,
        offset,
        size
    );

    let client = http_client();
    let response = authenticated(client.get(&request_url))
        .header("X-File-Offset", offset.to_string())
        .header("X-File-Size", size.to_string())
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;

    Ok(bytes.to_vec())
}

pub async fn create_directory(
    base_url: &str,
    path: &str,
    mode: u32,
) -> Result<RemoteMetadata, reqwest::Error> {
    let request_url = endpoint_url(base_url, "mkdir", path);
    log::debug!("Requesting directory creation: POST {}", request_url);

    let client = http_client();
    let response = add_optional_metadata_headers(
        authenticated(client.post(&request_url)),
        Some(mode),
        None,
        None,
        None,
    )
    .send()
    .await?;

    response.error_for_status()?.json::<RemoteMetadata>().await
}

// Sends an empty file to the server
pub async fn create_file(
    base_url: &str,
    path: &str,
    mode: u32,
) -> Result<RemoteMetadata, reqwest::Error> {
    let request_url = endpoint_url(base_url, "files", path);

    log::debug!("API: Creating new empty file via PUT {}", request_url);

    let client = http_client();
    let request = authenticated(client.put(&request_url))
        .header("X-File-Offset", "0")
        .body(vec![]);
    let response = add_optional_metadata_headers(request, Some(mode), None, None, None)
        .send()
        .await?;

    response.error_for_status()?.json::<RemoteMetadata>().await
}

// Sends a chunk of bytes to the server at a specific offset
pub async fn write_file(
    base_url: &str,
    path: &str,
    data: &[u8],
    offset: u64,
) -> Result<RemoteMetadata, reqwest::Error> {
    let request_url = endpoint_url(base_url, "files", path);

    log::debug!(
        "API: Writing {} bytes at offset {} to {}",
        data.len(),
        offset,
        request_url
    );

    let client = http_client();
    let response = authenticated(client.put(&request_url))
        .header("X-File-Offset", offset.to_string())
        .body(data.to_vec())
        .send()
        .await?;

    response.error_for_status()?.json::<RemoteMetadata>().await
}

// Asks the server to resize a file without sending file contents.
pub async fn resize_file(
    base_url: &str,
    path: &str,
    size: u64,
) -> Result<RemoteMetadata, reqwest::Error> {
    let request_url = endpoint_url(base_url, "files", path);

    log::debug!("API: Resizing {} to {} bytes", request_url, size);

    let client = http_client();
    let response = authenticated(client.put(&request_url))
        .header("X-File-Truncate", size.to_string())
        .body(vec![])
        .send()
        .await?;

    response.error_for_status()?.json::<RemoteMetadata>().await
}

pub async fn overwrite_file(
    base_url: &str,
    path: &str,
    mode: Option<u32>,
) -> Result<RemoteMetadata, reqwest::Error> {
    let request_url = endpoint_url(base_url, "files", path);

    log::debug!("API: Overwriting {}", request_url);

    let client = http_client();
    let request = authenticated(client.put(&request_url))
        .header("X-File-Truncate", "0")
        .body(Vec::new());
    let response = add_optional_metadata_headers(request, mode, None, None, None)
        .send()
        .await?;

    response.error_for_status()?.json::<RemoteMetadata>().await
}

pub async fn update_metadata(
    base_url: &str,
    path: &str,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    modified_at: Option<SystemTime>,
) -> Result<RemoteMetadata, reqwest::Error> {
    let request_url = endpoint_url(base_url, "metadata", path);

    log::debug!("API: Updating metadata for {}", request_url);

    let client = http_client();
    let response = add_optional_metadata_headers(
        authenticated(client.patch(&request_url)),
        mode,
        uid,
        gid,
        modified_at,
    )
    .send()
    .await?;

    response.error_for_status()?.json::<RemoteMetadata>().await
}

pub async fn delete_file(base_url: &str, path: &str) -> Result<(), reqwest::Error> {
    let request_url = endpoint_url(base_url, "files", path);

    log::debug!("API: Deleting {}", request_url);

    let client = http_client();
    let response = authenticated(client.delete(&request_url)).send().await?;

    response.error_for_status()?;
    Ok(())
}

pub async fn delete_directory(base_url: &str, path: &str) -> Result<(), reqwest::Error> {
    let request_url = endpoint_url(base_url, "directories", path);

    log::debug!("API: Deleting directory {}", request_url);

    let client = http_client();
    let response = authenticated(client.delete(&request_url)).send().await?;

    response.error_for_status()?;
    Ok(())
}

#[derive(Serialize)]
struct RenameRequest<'a> {
    from: &'a str,
    to: &'a str,
    replace_if_exists: bool,
}

pub async fn rename_file(
    base_url: &str,
    from: &str,
    to: &str,
    replace_if_exists: bool,
) -> Result<(), reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let request_url = format!("{}/rename", normalized_base);
    let normalized_from = from.trim_start_matches('/');
    let normalized_to = to.trim_start_matches('/');

    log::debug!(
        "API: Renaming {} to {} via POST {}",
        normalized_from,
        normalized_to,
        request_url
    );

    let client = http_client();
    let response = authenticated(client.post(&request_url))
        .json(&RenameRequest {
            from: normalized_from,
            to: normalized_to,
            replace_if_exists,
        })
        .send()
        .await?;

    response.error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{encode_api_path, endpoint_url};

    #[test]
    fn api_paths_encode_reserved_and_unicode_characters_per_component() {
        assert_eq!(
            encode_api_path("/docs/hash# percent% question?.txt"),
            "docs/hash%23%20percent%25%20question%3F.txt"
        );
        assert_eq!(
            encode_api_path("caffè/東京.txt"),
            "caff%C3%A8/%E6%9D%B1%E4%BA%AC.txt"
        );
    }

    #[test]
    fn endpoint_url_preserves_hierarchy_and_root_trailing_slash() {
        assert_eq!(
            endpoint_url("http://localhost:3000/", "files", "/a/b.txt"),
            "http://localhost:3000/files/a/b.txt"
        );
        assert_eq!(
            endpoint_url("http://localhost:3000", "list", ""),
            "http://localhost:3000/list/"
        );
    }
}
