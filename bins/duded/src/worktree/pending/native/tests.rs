use super::*;
use crate::worktree::pending::settled;

fn pending() -> (tempfile::TempDir, LeaseView) {
    let temp = tempfile::tempdir().unwrap();
    let origin = temp.path().join("origin");
    std::fs::create_dir_all(origin.join(".git")).unwrap();
    let worktree = dudes_types::WorktreeId::from_bytes([1; 16]);
    let path = temp.path().join("copies").join(worktree.to_string());
    let identity = prepare(&origin, &path).unwrap();
    (
        temp,
        LeaseView {
            worktree,
            origin,
            path,
            branch: format!("dudes/{}", worktree.short()),
            base: dudes_types::GitCommit::parse("0123456789abcdef0123456789abcdef01234567")
                .unwrap(),
            identity: Some(identity),
        },
    )
}

#[test]
fn empty_and_absent_roots_converge_without_directory_scans() {
    let (_temp, lease) = pending();
    discard(&lease).unwrap();
    settled(&lease).unwrap();
    discard(&lease).unwrap();
    settled(&lease).unwrap();
}

#[test]
fn a_plausible_replacement_is_retained_on_failure_and_restart() {
    let (temp, lease) = pending();
    let saved = temp.path().join("saved");
    std::fs::rename(&lease.path, &saved).unwrap();
    std::fs::create_dir(&lease.path).unwrap();
    std::fs::write(lease.path.join("keep"), "replacement").unwrap();
    std::fs::write(
        lease.path.join(".git"),
        format!(
            "gitdir: {}\n",
            lease
                .origin
                .join(".git/worktrees")
                .join(lease.path.file_name().unwrap())
                .display()
        ),
    )
    .unwrap();
    assert!(discard(&lease).is_err());
    assert!(discard(&lease.clone()).is_err());
    assert_eq!(
        std::fs::read_to_string(lease.path.join("keep")).unwrap(),
        "replacement"
    );
    assert!(saved.is_dir());
}

#[test]
fn a_replacement_between_check_and_move_is_retained_after_the_move() {
    let (temp, lease) = pending();
    let saved = temp.path().join("saved");
    let result = discard_with(
        &lease,
        || {
            std::fs::rename(&lease.path, &saved).unwrap();
            std::fs::create_dir(&lease.path).unwrap();
            std::fs::write(lease.path.join("keep"), "replacement").unwrap();
        },
        || {},
    );
    assert!(result.is_err());
    let hold = cleanup_path(&lease).unwrap();
    assert_eq!(
        std::fs::read_to_string(hold.join("keep")).unwrap(),
        "replacement"
    );
    assert!(discard(&lease).is_err());
    assert!(saved.is_dir());
}

#[test]
fn a_crash_after_the_cleanup_move_recovers_from_the_lease() {
    let (_temp, lease) = pending();
    std::fs::write(lease.path.join("owned"), "owned").unwrap();
    let crash = std::panic::catch_unwind(|| {
        let _ = discard_with(
            &lease,
            || {},
            || panic!("simulated crash after durable move"),
        );
    });
    assert!(crash.is_err());
    assert!(!lease.path.exists());
    assert!(cleanup_path(&lease).unwrap().join("owned").is_file());
    discard(&lease).unwrap();
    settled(&lease).unwrap();
}

#[test]
fn swapping_the_cleanup_name_cannot_redirect_descriptor_removal() {
    let (temp, lease) = pending();
    let saved = temp.path().join("moved-again");
    let hold = cleanup_path(&lease).unwrap();
    std::fs::write(lease.path.join("owned"), "owned").unwrap();
    let result = discard_with(
        &lease,
        || {},
        || {
            std::fs::rename(&hold, &saved).unwrap();
            std::fs::create_dir(&hold).unwrap();
            std::fs::write(hold.join("keep"), "replacement").unwrap();
        },
    );
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(hold.join("keep")).unwrap(),
        "replacement"
    );
    assert!(!saved.join("owned").exists());
    assert!(settled(&lease).is_err());
}

#[test]
fn an_inode_match_without_the_birth_identity_is_refused() {
    let (_temp, mut lease) = pending();
    lease.identity.as_mut().unwrap().root.created_nanos ^= 1;
    assert!(discard(&lease).is_err());
    assert!(lease.path.is_dir());
}

#[test]
fn unknown_git_registration_entries_retain_the_root_and_metadata() {
    let (_temp, lease) = pending();
    let admin = lease
        .origin
        .join(".git/worktrees")
        .join(lease.path.file_name().unwrap());
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::write(admin.join("unknown"), "keep").unwrap();
    assert!(discard(&lease).unwrap_err().contains("unproved"));
    assert_eq!(
        std::fs::read_to_string(admin.join("unknown")).unwrap(),
        "keep"
    );
    assert!(lease.path.exists());
}

#[test]
fn a_missing_parent_does_not_turn_permission_errors_into_absence() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("denied");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = absent(&parent.join("copy"));
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
}

#[test]
fn preparation_refuses_an_existing_directory_or_symlink() {
    let (temp, lease) = pending();
    std::fs::write(lease.path.join("keep"), "original").unwrap();
    assert!(prepare(&lease.origin, &lease.path).is_err());
    assert_eq!(
        std::fs::read_to_string(lease.path.join("keep")).unwrap(),
        "original"
    );
    let link = temp.path().join("copies/link");
    std::os::unix::fs::symlink(&lease.path, &link).unwrap();
    assert!(prepare(&lease.origin, &link).is_err());
    assert_eq!(
        std::fs::read_to_string(lease.path.join("keep")).unwrap(),
        "original"
    );
}

#[test]
fn sibling_git_metadata_is_neither_authority_nor_cleanup_input() {
    let (_temp, lease) = pending();
    let sibling = lease.origin.join(".git/worktrees/another-copy");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("keep"), "another copy").unwrap();
    discard(&lease).unwrap();
    assert_eq!(
        std::fs::read_to_string(sibling.join("keep")).unwrap(),
        "another copy"
    );
}

#[test]
fn a_gitfile_symlink_is_retained_without_following_it() {
    let (temp, lease) = pending();
    let foreign = temp.path().join("foreign");
    std::fs::write(&foreign, "keep").unwrap();
    std::os::unix::fs::symlink(&foreign, lease.path.join(".git")).unwrap();
    assert!(discard(&lease).is_err());
    assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "keep");
    assert!(lease.path.is_dir());
}

#[test]
fn a_mounted_descendant_is_refused_before_its_foreign_sentinel_is_removed() {
    let Some(fixture) = std::env::var_os("DUDES_PENDING_MOUNT_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(fixture).join("root");
    let sentinel = root.join("mounted/sentinel");
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "foreign sentinel"
    );
    let directory = open(&root).unwrap();
    assert!(
        child(&directory, "mounted").is_err(),
        "a mounted child was admitted for traversal"
    );
    assert!(remove_contents(&directory).is_err());
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "foreign sentinel"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_kernel_refuses_the_procfs_mount_without_descent() {
    let root = open(Path::new("/")).unwrap();
    let error = open_on_mount(&root, "proc", DIRECTORY).unwrap_err();
    assert_eq!(error, rustix::io::Errno::XDEV);
}
