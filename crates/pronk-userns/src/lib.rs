//! Fail-closed host-identity checks across Linux user namespaces.

use std::fs;
use std::os::unix::fs::MetadataExt;

const USER_NAMESPACE_PATH: &str = "/proc/self/ns/user";
// Linux UAPI `USER_NS_INIT_INO` from <linux/nsfs.h>.
const INITIAL_USER_NAMESPACE_INODE: u64 = 0xefff_fffd;

/// Return whether a file owner observed by this process represents host UID 0.
///
/// This deliberately supports only the initial user namespace. File ownership
/// returned by `stat` is relative to the caller's user namespace, and an
/// overflow UID carries no information about which unmapped UID owned the
/// inode. Missing or unexpected namespace state therefore fails closed.
pub fn is_host_root_owner(owner_uid: u32) -> bool {
    fs::metadata(USER_NAMESPACE_PATH)
        .is_ok_and(|metadata| is_host_root_owner_in_namespace(owner_uid, metadata.ino()))
}

fn is_host_root_owner_in_namespace(owner_uid: u32, user_namespace_inode: u64) -> bool {
    owner_uid == 0 && user_namespace_inode == INITIAL_USER_NAMESPACE_INODE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_root_only_in_the_initial_user_namespace() {
        assert!(is_host_root_owner_in_namespace(0, 0xefff_fffd));
        assert!(!is_host_root_owner_in_namespace(0, 4_026_531_836));
        assert!(!is_host_root_owner_in_namespace(0, 4_026_531_838));
        assert!(!is_host_root_owner_in_namespace(65_534, 0xefff_fffd));
        assert!(!is_host_root_owner_in_namespace(65_534, 4_026_531_836));
    }

    #[test]
    fn rejects_non_root_owners_in_every_namespace() {
        assert!(!is_host_root_owner(1_000));
        assert!(!is_host_root_owner(65_534));
    }
}
