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
