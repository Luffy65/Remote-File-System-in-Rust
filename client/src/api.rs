use log;
use reqwest;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct DirectoryEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub size: u64,
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
        let err_msg = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
        log::error!("Error fetching directory list: {} - {}", status, err_msg);
        // Return an error by making a failing request to get a proper reqwest::Error
        reqwest::get("http://invalid-non-existent-server-domain-12345").await.map(|_| vec![]).map_err(|e| e)
    }
}

pub async fn read_file(base_url: &str, path: &str) -> Result<Vec<u8>, reqwest::Error> {
    // Normalize paths
    let normalized_base_url = base_url.trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/');

    // Construct the endpoint for file reading
    let request_url = format!("{}/files/{}", normalized_base_url, normalized_path);

    log::debug!("Requesting file content from URL: {}", request_url);

    // Fetch the response and automatically convert HTTP errors (like 404) into reqwest::Error
    let response = reqwest::get(&request_url).await?.error_for_status()?;

    // Read the body as bytes
    let bytes = response.bytes().await?;

    Ok(bytes.to_vec())
}
