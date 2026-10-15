use std::cell::RefCell;
use std::fmt::Write as _;
use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use super::tests::budget;
use super::*;

const OBJECTS: [(&str, &[u8]); 3] = [
    (
        "e0d22c0f8aa0298a5d1f9da8583de1c1ef2a88e1",
        &[
            120, 156, 75, 202, 201, 79, 82, 48, 50, 98, 40, 40, 202, 79, 78, 45, 46, 86, 0, 210,
            249, 105, 10, 105, 153, 21, 37, 165, 69, 169, 92, 0, 160, 14, 10, 154,
        ],
    ),
    (
        "0ffac8e2260e6b97b80e26f6d1cb336183318f58",
        &[
            120, 156, 43, 41, 74, 77, 85, 48, 54, 103, 48, 52, 48, 48, 51, 49, 81, 40, 40, 202,
            207, 79, 211, 43, 169, 40, 97, 120, 112, 73, 135, 191, 107, 129, 102, 87, 172, 252,
            220, 21, 17, 182, 15, 15, 190, 215, 234, 120, 8, 0, 82, 5, 17, 130,
        ],
    ),
    (
        "6f400e6ffbd66dd645cbb99e6d3616499afcded5",
        &[
            120, 156, 141, 141, 75, 10, 194, 48, 16, 64, 93, 231, 20, 179, 23, 36, 31, 29, 71, 40,
            210, 35, 120, 133, 36, 157, 193, 64, 99, 74, 152, 74, 143, 111, 245, 4, 190, 205, 219,
            60, 120, 185, 213, 90, 20, 220, 153, 14, 218, 153, 193, 138, 196, 76, 236, 61, 90, 198,
            116, 187, 38, 178, 236, 81, 112, 114, 57, 133, 128, 142, 66, 112, 36, 23, 50, 113, 213,
            103, 235, 240, 232, 173, 9, 12, 203, 87, 35, 111, 177, 46, 51, 159, 202, 235, 29, 231,
            50, 221, 193, 194, 209, 238, 152, 252, 187, 40, 255, 219, 27, 41, 155, 174, 157, 205,
            7, 136, 12, 50, 131,
        ],
    ),
];

thread_local! {
    static CLEANUP_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub(super) fn after_unlink(entry: &OwnedName) {
    if entry.kind != FileType::RegularFile || !hex(entry.name.as_bytes(), 38) {
        return;
    }
    let root = CLEANUP_ROOT.with(|slot| slot.borrow_mut().take());
    if let Some(root) = root {
        barrier(&root);
    }
}

fn proof_root() -> PathBuf {
    let root = PathBuf::from(std::env::var_os("DUDES_SHADOW_PROOF_ROOT").unwrap());
    assert!(root.is_absolute());
    assert_eq!(fs::canonicalize(&root).unwrap(), root);
    assert!(
        root.file_name()
            .unwrap()
            .as_bytes()
            .starts_with(b"dudes-shadow-process-")
    );
    assert_eq!(
        fs::read(root.join(".process-proof")).unwrap(),
        b"scratch only\n"
    );
    root
}

fn barrier(root: &Path) {
    fs::write(root.join("ready"), b"ready\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !root.join("continue").exists() {
        assert!(Instant::now() < deadline, "process proof barrier expired");
        thread::sleep(Duration::from_millis(10));
    }
}

fn fixture(root: &Path) -> PathBuf {
    let tree = root.join("tree");
    fs::create_dir_all(tree.join(".git/refs/heads")).unwrap();
    fs::write(tree.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    fs::write(
        tree.join(".git/refs/heads/main"),
        format!("{}\n", OBJECTS[2].0),
    )
    .unwrap();
    let mut original = String::new();
    for (id, bytes) in OBJECTS {
        let directory = tree.join(".git/objects").join(&id[..2]);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(&id[2..]);
        fs::write(&path, bytes).unwrap();
        let meta = fs::metadata(path).unwrap();
        writeln!(
            original,
            "{id} {} {} {}",
            meta.dev(),
            meta.ino(),
            meta.nlink()
        )
        .unwrap();
    }
    fs::write(root.join("source-before"), original).unwrap();
    tree
}

#[test]
#[ignore = "requires an external process owner and isolated scratch directory"]
fn content_shadow_process_child() {
    let root = proof_root();
    let mode = std::env::var("DUDES_SHADOW_PROOF_MODE").unwrap();
    assert!(matches!(mode.as_str(), "staging" | "cleanup" | "normal"));
    let tree = fixture(&root);
    let shadow = ContentShadow::build_with(
        &root.join("scratch"),
        "sample",
        &Tree::Checkout(tree),
        None,
        &budget(),
        |stage, _| {
            if mode == "staging" && stage == Stage::DestinationLinked {
                barrier(&root);
            }
        },
    )
    .unwrap();
    assert_eq!(shadow.verify(&budget()), Ok(()));
    if mode == "cleanup" {
        CLEANUP_ROOT.with(|slot| *slot.borrow_mut() = Some(root.clone()));
    }
    drop(shadow);
    assert!(!root.join("scratch/sample").exists());
    fs::write(root.join("completed"), b"completed\n").unwrap();
}

#[test]
#[ignore = "requires an external process owner and isolated scratch directory"]
fn content_shadow_reclaim_process_child() {
    let root = proof_root();
    let count = ContentShadow::reclaim(&root.join("scratch"), &budget()).unwrap();
    fs::write(root.join("reclaimed"), format!("{count}\n")).unwrap();
}

#[test]
#[ignore = "requires a scratch directory on a different volume than the temporary checkout"]
fn content_shadow_refuses_a_scratch_directory_on_another_device() {
    let scratch = PathBuf::from(std::env::var_os("DUDES_SHADOW_OTHER_DEVICE").unwrap());
    let fixture = super::tests::Fixture::new();
    let leaf = fixture.leaf();
    assert_ne!(
        fs::metadata(&scratch).unwrap().dev(),
        fs::metadata(&leaf).unwrap().dev()
    );
    let before = fs::metadata(&leaf).unwrap();
    let built = ContentShadow::build(
        &scratch,
        "sample",
        &Tree::Checkout(fixture.tree.clone()),
        None,
        &budget(),
    );
    assert_eq!(built.err(), Some(DiffRefusal::UnsupportedStorage));
    assert!(!scratch.join("sample").exists());
    let after = fs::metadata(&leaf).unwrap();
    assert_eq!(
        (after.ino(), after.nlink(), fs::read(&leaf).unwrap()),
        (before.ino(), before.nlink(), b"object bytes".to_vec())
    );
}
