//! Shared wire-format definitions for remote-fs protocol version 1.
//!
//! Platform adapters translate these transport types into native FUSE or
//! WinFSP types at their boundary. This crate intentionally contains no I/O or
//! filesystem policy.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: &str = "1";
pub const PROTOCOL_VERSION_HEADER: &str = "X-Remote-Fs-Protocol-Version";

pub mod headers {
    pub const FILE_OFFSET: &str = "X-File-Offset";
    pub const FILE_SIZE: &str = "X-File-Size";
    pub const FILE_TRUNCATE: &str = "X-File-Truncate";
    pub const FILE_MODE: &str = "X-File-Mode";
    pub const FILE_UID: &str = "X-File-Uid";
    pub const FILE_GID: &str = "X-File-Gid";
    pub const FILE_MTIME: &str = "X-File-Mtime";
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RemoteMetadata {
    #[serde(rename = "type")]
    pub type_: String,
    pub size: u64,
    pub modified_at: String,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RenameRequest {
    pub from: String,
    pub to: String,
    #[serde(default = "replace_if_exists_by_default")]
    pub replace_if_exists: bool,
}

fn replace_if_exists_by_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_defaults_to_replacing_the_destination() {
        let request: RenameRequest =
            serde_json::from_str(r#"{"from":"old.txt","to":"new.txt"}"#).unwrap();
        assert!(request.replace_if_exists);
    }
}
