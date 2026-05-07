use log;
use reqwest;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug)]
pub struct DirectoryEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub size: u64,
    #[allow(dead_code)]
    pub modified_at: String,
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

    let response = reqwest::get(&request_url).await?;
    log::debug!("Received response: {:?}", response.status());

    if response.status().is_success() {
        let entries = response.json::<Vec<DirectoryEntry>>().await?;
        Ok(entries)
    } else {
        let status = response.status();
        let err_msg = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        log::error!("Error fetching directory list: {} - {}", status, err_msg);
        // Return an error by making a failing request to get a proper reqwest::Error
        reqwest::get("http://invalid-non-existent-server-domain-12345")
            .await
            .map(|_| vec![])
            .map_err(|e| e)
    }
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

    let client = reqwest::Client::new();
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

pub async fn create_directory(base_url: &str, path: &str) -> Result<(), reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');

    let request_url = format!("{}/mkdir/{}", normalized_base, normalized_path);
    log::debug!("Requesting directory creation: POST {}", request_url);

    // Create a reqwest client to send the POST request
    let client = reqwest::Client::new();
    let response = client.post(&request_url).send().await?;

    // If the server returns an error (like 404 or 500), this turns it into a Rust Error
    response.error_for_status()?;

    Ok(())
}

// Sends an empty file to the server
pub async fn create_file(base_url: &str, path: &str) -> Result<(), reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/files/{}", normalized_base, normalized_path);

    log::debug!("API: Creating new empty file via PUT {}", request_url);

    let client = reqwest::Client::new();
    let response = client
        .put(&request_url)
        .header("X-File-Offset", "0")
        .body(vec![]) // Empty body to initialize the file
        .send()
        .await?;

    response.error_for_status()?;
    Ok(())
}

// Sends a chunk of bytes to the server at a specific offset
pub async fn write_file(
    base_url: &str,
    path: &str,
    data: &[u8],
    offset: u64,
) -> Result<(), reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/files/{}", normalized_base, normalized_path);

    log::debug!(
        "API: Writing {} bytes at offset {} to {}",
        data.len(),
        offset,
        request_url
    );

    let client = reqwest::Client::new();
    let response = client
        .put(&request_url)
        .header("X-File-Offset", offset.to_string())
        .body(data.to_vec())
        .send()
        .await?;

    response.error_for_status()?;
    Ok(())
}

// Asks the server to resize a file without sending file contents.
pub async fn resize_file(base_url: &str, path: &str, size: u64) -> Result<(), reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/files/{}", normalized_base, normalized_path);

    log::debug!("API: Resizing {} to {} bytes", request_url, size);

    let client = reqwest::Client::new();
    let response = client
        .put(&request_url)
        .header("X-File-Truncate", size.to_string())
        .body(vec![])
        .send()
        .await?;

    response.error_for_status()?;
    Ok(())
}

pub async fn delete_file(base_url: &str, path: &str) -> Result<(), reqwest::Error> {
    let normalized_base = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');
    let request_url = format!("{}/files/{}", normalized_base, normalized_path);

    log::debug!("API: Deleting {}", request_url);

    let client = reqwest::Client::new();
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

    let client = reqwest::Client::new();
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
