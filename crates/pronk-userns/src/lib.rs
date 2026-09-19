//! Fail-closed host-identity checks across Linux user namespaces.

use std::fs;
use std::os::unix::fs::MetadataExt;

const USER_NAMESPACE_PATH: &str = "/proc/self/ns/user";
const USER_NAMESPACE_MAP_PATH: &str = "/proc/self/uid_map";
const OVERFLOW_UID_PATH: &str = "/proc/sys/kernel/overflowuid";
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

/// Return whether an installed system file has an owner outside the service's
/// control.
///
/// A per-user systemd service that requests filesystem or network namespacing
/// runs in a singleton identity user namespace. Host UID 0 is deliberately
/// unmapped there, so `stat` reports the overflow UID for root-owned package
/// files. Accept that representation only when the namespace maps exactly the
/// effective service UID to the same host UID. Callers must additionally
/// require trusted paths and reject group- or world-writable files.
pub fn is_system_file_owner(owner_uid: u32) -> bool {
    let Ok(namespace) = fs::metadata(USER_NAMESPACE_PATH) else {
        return false;
    };
    if namespace.ino() == INITIAL_USER_NAMESPACE_INODE {
        return owner_uid == 0;
    }

    let Ok(process) = fs::metadata("/proc/self") else {
        return false;
    };
    let Ok(uid_map) = fs::read_to_string(USER_NAMESPACE_MAP_PATH) else {
        return false;
    };
    let Ok(overflow_uid) = fs::read_to_string(OVERFLOW_UID_PATH) else {
        return false;
    };
    let Ok(overflow_uid) = overflow_uid.trim().parse() else {
        return false;
    };

    is_unmapped_system_owner(owner_uid, overflow_uid, process.uid(), &uid_map)
}

fn is_unmapped_system_owner(
    owner_uid: u32,
    overflow_uid: u32,
    service_uid: u32,
    uid_map: &str,
) -> bool {
    owner_uid == overflow_uid
        && service_uid != 0
        && service_uid != overflow_uid
        && has_single_identity_mapping(uid_map, service_uid)
}

fn is_host_root_owner_in_namespace(owner_uid: u32, user_namespace_inode: u64) -> bool {
    owner_uid == 0 && user_namespace_inode == INITIAL_USER_NAMESPACE_INODE
}

fn has_single_identity_mapping(uid_map: &str, effective_uid: u32) -> bool {
    let mut lines = uid_map.lines().filter(|line| !line.trim().is_empty());
    let Some(line) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false;
    }
    let mut fields = line.split_whitespace();
    let Some(inside) = fields.next().and_then(|field| field.parse::<u32>().ok()) else {
        return false;
    };
    let Some(outside) = fields.next().and_then(|field| field.parse::<u32>().ok()) else {
        return false;
    };
    let Some(length) = fields.next().and_then(|field| field.parse::<u32>().ok()) else {
        return false;
    };

    fields.next().is_none() && inside == effective_uid && outside == effective_uid && length == 1
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

    #[test]
    fn recognizes_only_a_single_identity_mapping() {
        assert!(has_single_identity_mapping("4153 4153 1\n", 4153));
        assert!(!has_single_identity_mapping("0 4153 1\n", 4153));
        assert!(!has_single_identity_mapping("4153 1000 1\n", 4153));
        assert!(!has_single_identity_mapping("4153 4153 2\n", 4153));
        assert!(!has_single_identity_mapping(
            "4153 4153 1\n5000 5000 1\n",
            4153
        ));
        assert!(!has_single_identity_mapping("not a mapping\n", 4153));
    }

    #[test]
    fn unmapped_system_owner_cannot_be_the_service_identity() {
        assert!(is_unmapped_system_owner(
            65_534,
            65_534,
            4153,
            "4153 4153 1\n"
        ));
        assert!(!is_unmapped_system_owner(
            65_534,
            65_534,
            65_534,
            "65534 65534 1\n"
        ));
        assert!(!is_unmapped_system_owner(65_534, 65_534, 0, "0 0 1\n"));
        assert!(!is_unmapped_system_owner(
            4153,
            65_534,
            4153,
            "4153 4153 1\n"
        ));
    }

    #[test]
    #[ignore = "requires an installed backend and a singleton user namespace"]
    fn installed_backend_owner_is_trusted_in_single_identity_namespace() {
        assert_ne!(
            fs::metadata(USER_NAMESPACE_PATH).unwrap().ino(),
            INITIAL_USER_NAMESPACE_INODE
        );
        let metadata = fs::metadata("/usr/lib/pronk/backends.d/chromiacast.toml").unwrap();
        assert!(is_system_file_owner(metadata.uid()));
    }
}
