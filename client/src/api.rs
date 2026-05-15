use log;
use reqwest;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize, Debug, Clone)]
pub struct DirectoryEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub size: u64,
    pub modified_at: String,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RemoteMetadata {
    #[serde(rename = "type")]
    pub type_: String,
    pub size: u64,
    pub modified_at: String,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build()
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
    // Normalize path: remove leading slash if present, no trailing slash needed.
    let normalized_path = path.trim_start_matches('/');
    let path_segment = normalized_path.to_string();

    // Normalize base_url: ensure it does not end with a slash before appending segments.
    let normalized_base_url = base_url.trim_end_matches('/');

    let request_url = format!("{}/list/{}", normalized_base_url, path_segment);

    log::debug!("Requesting directory list from URL: {}", request_url);

    let response = http_client()?.get(&request_url).send().await?;
    log::debug!("Received response: {:?}", response.status());

    let entries = response
        .error_for_status()?
        .json::<Vec<DirectoryEntry>>()
        .await?;
    Ok(entries)
}

// Requests only one byte range from the server instead of downloading the whole file.
pub async fn read_file(
    base_url: &str,
    path: &str,
    offset: u64,
    size: u32,
) -> Result<Vec<u8>, reqwest::Error> {
    let normalized_base_url = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/files/{}", normalized_base_url, normalized_path);

    log::debug!(
        "Requesting file range from URL: {} (offset={}, size={})",
        request_url,
        offset,
        size
    );

    let client = http_client()?;
    let response = client
        .get(&request_url)
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
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');

    let request_url = format!("{}/mkdir/{}", normalized_base, normalized_path);
    log::debug!("Requesting directory creation: POST {}", request_url);

    let client = http_client()?;
    let response =
        add_optional_metadata_headers(client.post(&request_url), Some(mode), None, None, None)
            .send()
            .await?;

    Ok(response
        .error_for_status()?
        .json::<RemoteMetadata>()
        .await?)
}

// Sends an empty file to the server
pub async fn create_file(
    base_url: &str,
    path: &str,
    mode: u32,
) -> Result<RemoteMetadata, reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/files/{}", normalized_base, normalized_path);

    log::debug!("API: Creating new empty file via PUT {}", request_url);

    let client = http_client()?;
    let request = client
        .put(&request_url)
        .header("X-File-Offset", "0")
        .body(vec![]);
    let response = add_optional_metadata_headers(request, Some(mode), None, None, None)
        .send()
        .await?;

    Ok(response
        .error_for_status()?
        .json::<RemoteMetadata>()
        .await?)
}

// Sends a chunk of bytes to the server at a specific offset
pub async fn write_file(
    base_url: &str,
    path: &str,
    data: &[u8],
    offset: u64,
) -> Result<RemoteMetadata, reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/files/{}", normalized_base, normalized_path);

    log::debug!(
        "API: Writing {} bytes at offset {} to {}",
        data.len(),
        offset,
        request_url
    );

    let client = http_client()?;
    let response = client
        .put(&request_url)
        .header("X-File-Offset", offset.to_string())
        .body(data.to_vec())
        .send()
        .await?;

    Ok(response
        .error_for_status()?
        .json::<RemoteMetadata>()
        .await?)
}

// Asks the server to resize a file without sending file contents.
pub async fn resize_file(
    base_url: &str,
    path: &str,
    size: u64,
) -> Result<RemoteMetadata, reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/files/{}", normalized_base, normalized_path);

    log::debug!("API: Resizing {} to {} bytes", request_url, size);

    let client = http_client()?;
    let response = client
        .put(&request_url)
        .header("X-File-Truncate", size.to_string())
        .body(vec![])
        .send()
        .await?;

    Ok(response
        .error_for_status()?
        .json::<RemoteMetadata>()
        .await?)
}

pub async fn update_metadata(
    base_url: &str,
    path: &str,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    modified_at: Option<SystemTime>,
) -> Result<RemoteMetadata, reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/metadata/{}", normalized_base, normalized_path);

    log::debug!("API: Updating metadata for {}", request_url);

    let client = http_client()?;
    let response =
        add_optional_metadata_headers(client.patch(&request_url), mode, uid, gid, modified_at)
            .send()
            .await?;

    Ok(response
        .error_for_status()?
        .json::<RemoteMetadata>()
        .await?)
}

pub async fn delete_file(base_url: &str, path: &str) -> Result<(), reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/files/{}", normalized_base, normalized_path);

    log::debug!("API: Deleting {}", request_url);

    let client = http_client()?;
    let response = client.delete(&request_url).send().await?;

    response.error_for_status()?;
    Ok(())
}

#[derive(Serialize)]
struct RenameRequest<'a> {
    from: &'a str,
    to: &'a str,
}

pub async fn rename_file(base_url: &str, from: &str, to: &str) -> Result<(), reqwest::Error> {
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

    let client = http_client()?;
    let response = client
        .post(&request_url)
        .json(&RenameRequest {
            from: normalized_from,
            to: normalized_to,
        })
        .send()
        .await?;

    response.error_for_status()?;
    Ok(())
}
