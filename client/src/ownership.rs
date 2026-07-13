//! Translation between optional remote ownership and the local mount owner.

/// Preserve real ownership from Unix servers, but use the mounting user when
/// the server platform has no UID/GID concept.
pub(crate) fn remote_or_mounting_user(remote_id: Option<u32>, mounting_id: u32) -> u32 {
    remote_id.unwrap_or(mounting_id)
}

#[cfg(test)]
mod tests {
    use super::remote_or_mounting_user;

    #[test]
    fn remote_ownership_wins_and_missing_ownership_uses_mounting_user() {
        assert_eq!(remote_or_mounting_user(Some(1001), 501), 1001);
        assert_eq!(remote_or_mounting_user(None, 501), 501);
    }
}
