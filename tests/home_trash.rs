//! Run each filesystem scenario in its own process: the trash location is
//! selected through environment variables, which must not race other tests.
#![cfg(all(target_os = "linux", feature = "freedesktop"))]

use std::{
    ffi::OsString,
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{symlink, MetadataExt, PermissionsExt},
        net::UnixListener,
    },
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, UNIX_EPOCH},
};

use trash::{
    freedesktop::{TrashContextExtFreedesktop, TrashLocation},
    os_limited::{list, restore_all},
    TrashContext, TrashItem,
};

fn run_case(case: &str, cross_filesystem: bool) {
    let root = tempfile::tempdir().unwrap();
    let other = if cross_filesystem {
        let dir = match tempfile::Builder::new().prefix("trash-home-test-").tempdir_in("/dev/shm") {
            Ok(dir) => dir,
            Err(error) => {
                eprintln!("skipping {case}: cannot create a second filesystem fixture: {error}");
                return;
            }
        };
        if fs::metadata(root.path()).unwrap().dev() == fs::metadata(dir.path()).unwrap().dev() {
            eprintln!("skipping {case}: /dev/shm and the test directory are on the same filesystem");
            return;
        }
        Some(dir)
    } else {
        None
    };
    let data = other.as_ref().map_or_else(|| root.path().join("data"), |dir| dir.path().join("data"));
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "isolated_worker", "--nocapture"])
        .env("TRASH_HOME_TEST_CASE", case)
        .env("TRASH_HOME_TEST_ROOT", root.path())
        .env("HOME", root.path())
        .env("XDG_DATA_HOME", &data)
        .env("TZ", "UTC")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{case} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn home_context() -> TrashContext {
    let mut context = TrashContext::new();
    context.set_trash_location(TrashLocation::Home);
    context
}

fn our_items(work: &Path) -> Vec<TrashItem> {
    list().unwrap().into_iter().filter(|item| item.original_path().starts_with(work)).collect()
}

fn payload(item: &TrashItem) -> PathBuf {
    let receipt = Path::new(&item.id);
    receipt.parent().unwrap().parent().unwrap().join("files").join(receipt.file_stem().unwrap())
}

fn empty_directory(path: &Path) -> bool {
    !path.exists() || fs::read_dir(path).unwrap().next().is_none()
}

#[test]
fn context_defaults_remain_per_filesystem_and_preferences_are_per_instance() {
    let mut home = TrashContext::new();
    assert_eq!(home.trash_location(), TrashLocation::PerFilesystem);
    assert_eq!(TrashContext::default().trash_location(), TrashLocation::PerFilesystem);
    home.set_trash_location(TrashLocation::Home);
    assert_eq!(home.trash_location(), TrashLocation::Home);
    assert_eq!(home.clone().trash_location(), TrashLocation::Home);
    assert_eq!(TrashContext::new().trash_location(), TrashLocation::PerFilesystem);
    home.set_trash_location(TrashLocation::PerFilesystem);
    assert_eq!(home.trash_location(), TrashLocation::PerFilesystem);
}

#[test]
fn home_only_round_trip_preserves_trees_symlinks_and_metadata_across_filesystems() {
    run_case("cross_round_trip", true);
}

#[test]
fn restore_collisions_and_failed_copies_keep_the_complete_trash_item() {
    run_case("restore_failures", true);
}

#[test]
fn unsupported_cross_filesystem_payload_leaves_the_source_and_no_partial_trash() {
    run_case("trash_copy_failure", true);
}

#[test]
fn long_and_non_unicode_names_round_trip_with_private_trash_metadata() {
    run_case("native_names", false);
}

#[test]
fn malformed_receipts_and_orphan_payloads_are_preserved() {
    run_case("external_entries", false);
}

#[test]
fn unusable_home_trash_fails_without_selecting_another_destination() {
    run_case("unusable_home", true);
}

#[test]
fn a_mounted_root_in_a_batch_is_rejected_before_any_item_moves() {
    run_case("root_preflight", true);
}

#[test]
fn changing_placement_does_not_hide_previously_trashed_items() {
    run_case("change_placement", true);
}

#[test]
fn isolated_worker() {
    let Some(case) = std::env::var_os("TRASH_HOME_TEST_CASE") else { return };
    let root = PathBuf::from(std::env::var_os("TRASH_HOME_TEST_ROOT").unwrap());
    let work = root.join("work");
    let trash = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap()).join("Trash");
    fs::create_dir(&work).unwrap();
    let context = home_context();
    match case.to_str().unwrap() {
        "cross_round_trip" => cross_round_trip(&work, &trash, &context),
        "restore_failures" => restore_failures(&work, &context),
        "trash_copy_failure" => trash_copy_failure(&work, &trash, &context),
        "native_names" => native_names(&work, &trash, &context),
        "external_entries" => external_entries(&work, &trash, &context),
        "root_preflight" => {
            let source = work.join("ordinary-file");
            fs::write(&source, "leave this in place").unwrap();
            assert!(matches!(
                context.delete_all([source.as_path(), Path::new("/dev/shm")]),
                Err(trash::Error::TargetedRoot)
            ));
            assert_eq!(fs::read_to_string(source).unwrap(), "leave this in place");
            assert!(empty_directory(&trash.join("files")));
            assert!(our_items(&work).is_empty());
        }
        "change_placement" => {
            let source = work.join("home-entry");
            fs::write(&source, "still discoverable").unwrap();
            let mut changed = context.clone();
            changed.delete(&source).unwrap();
            changed.set_trash_location(TrashLocation::PerFilesystem);
            assert_eq!(changed.trash_location(), TrashLocation::PerFilesystem);
            let items = our_items(&work);
            assert_eq!(items.len(), 1);
            assert!(Path::new(&items[0].id).starts_with(trash.join("info")));
            restore_all(items).unwrap();
            assert_eq!(fs::read_to_string(source).unwrap(), "still discoverable");
            assert!(our_items(&work).is_empty());
        }
        "unusable_home" => {
            fs::create_dir_all(trash.parent().unwrap()).unwrap();
            fs::write(&trash, "not a directory").unwrap();
            let source = work.join("source");
            fs::write(&source, "preserve me").unwrap();
            assert!(context.delete(&source).is_err());
            assert_eq!(fs::read_to_string(source).unwrap(), "preserve me");
            assert_eq!(fs::read_to_string(trash).unwrap(), "not a directory");
        }
        other => panic!("unknown scenario {other}"),
    }
}

fn cross_round_trip(work: &Path, trash: &Path, context: &TrashContext) {
    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    let child = tree.join("child");
    fs::write(&child, b"binary\0payload").unwrap();
    let modified = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    fs::File::open(&child).unwrap().set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(&tree, fs::Permissions::from_mode(0o750)).unwrap();
    let outside = work.join("outside");
    fs::write(&outside, "outside").unwrap();
    symlink(&outside, tree.join("external")).unwrap();
    let directory_link = work.join("directory-link");
    symlink(&tree, &directory_link).unwrap();
    let broken = work.join(OsString::from_vec(b"broken-\xff".to_vec()));
    symlink("absent", &broken).unwrap();
    let file = work.join("file");
    fs::write(&file, "regular file").unwrap();
    fs::File::open(&tree).unwrap().set_times(fs::FileTimes::new().set_modified(modified)).unwrap();

    context.delete_all([&tree, &directory_link, &broken, &file]).unwrap();
    for path in [&tree, &directory_link, &broken, &file] {
        assert!(path.symlink_metadata().is_err(), "source survived: {path:?}");
    }
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
    let items = our_items(work);
    assert_eq!(items.len(), 4);
    for item in &items {
        assert!(Path::new(&item.id).starts_with(trash.join("info")));
    }
    let tree_item = items.iter().find(|item| item.original_path() == tree).unwrap();
    assert_eq!(fs::read(payload(tree_item).join("child")).unwrap(), b"binary\0payload");
    assert_eq!(fs::read_link(payload(tree_item).join("external")).unwrap(), outside);
    assert_eq!(
        fs::read_link(payload(items.iter().find(|item| item.original_path() == directory_link).unwrap())).unwrap(),
        tree
    );
    assert_eq!(
        fs::read_link(payload(items.iter().find(|item| item.original_path() == broken).unwrap())).unwrap(),
        Path::new("absent")
    );
    restore_all(items).unwrap();
    assert_eq!(fs::read(&child).unwrap(), b"binary\0payload");
    assert_eq!(fs::metadata(&child).unwrap().modified().unwrap(), modified);
    assert_eq!(fs::metadata(&tree).unwrap().modified().unwrap(), modified);
    assert_eq!(fs::metadata(&child).unwrap().permissions().mode() & 0o777, 0o640);
    assert_eq!(fs::metadata(&tree).unwrap().permissions().mode() & 0o777, 0o750);
    assert_eq!(fs::read_link(tree.join("external")).unwrap(), outside);
    assert_eq!(fs::read_link(directory_link).unwrap(), tree);
    assert_eq!(fs::read_link(broken).unwrap(), Path::new("absent"));
    assert_eq!(fs::read_to_string(file).unwrap(), "regular file");
    assert!(our_items(work).is_empty());
}

fn restore_failures(work: &Path, context: &TrashContext) {
    let source = work.join("versioned");
    fs::write(&source, "first").unwrap();
    context.delete(&source).unwrap();
    let item = our_items(work).pop().unwrap();
    fs::write(&source, "second").unwrap();
    assert!(matches!(restore_all([item.clone()]), Err(trash::Error::RestoreCollision { .. })));
    assert_eq!(fs::read_to_string(&source).unwrap(), "second");
    assert_eq!(fs::read_to_string(payload(&item)).unwrap(), "first");
    assert!(Path::new(&item.id).exists());
    fs::remove_file(&source).unwrap();
    restore_all([item]).unwrap();
    assert_eq!(fs::read_to_string(source).unwrap(), "first");

    let tree = work.join("tree");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("data"), "complete backup").unwrap();
    context.delete(&tree).unwrap();
    let item = our_items(work).pop().unwrap();
    let socket = payload(&item).join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    assert!(restore_all([item.clone()]).is_err());
    assert!(!tree.exists(), "failed restore left a placeholder or partial destination");
    assert_eq!(fs::read_to_string(payload(&item).join("data")).unwrap(), "complete backup");
    assert!(Path::new(&item.id).exists());
    drop(listener);
    fs::remove_file(socket).unwrap();
    restore_all([item]).unwrap();
    assert_eq!(fs::read_to_string(tree.join("data")).unwrap(), "complete backup");
}

fn trash_copy_failure(work: &Path, trash: &Path, context: &TrashContext) {
    let source = work.join("tree");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("data"), "keep source intact").unwrap();
    let socket = source.join("socket");
    let _listener = UnixListener::bind(&socket).unwrap();
    assert!(context.delete(&source).is_err());
    assert_eq!(fs::read_to_string(source.join("data")).unwrap(), "keep source intact");
    assert!(socket.exists());
    assert!(empty_directory(&trash.join("files")));
    assert!(empty_directory(&trash.join("info")));
    assert!(our_items(work).is_empty());
}

fn native_names(work: &Path, trash: &Path, context: &TrashContext) {
    let long = work.join("x".repeat(255));
    let binary = work.join(OsString::from_vec(b"a=b %\xff".to_vec()));
    fs::write(&long, "long filename").unwrap();
    fs::write(&binary, "native filename").unwrap();
    context.delete_all([&long, &binary]).unwrap();
    let items = our_items(work);
    assert_eq!(items.len(), 2);
    for dir in [trash.to_owned(), trash.join("files"), trash.join("info")] {
        assert_eq!(fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
    }
    for item in &items {
        assert_eq!(fs::metadata(&item.id).unwrap().permissions().mode() & 0o777, 0o600);
        if cfg!(feature = "chrono") {
            assert!(item.time_deleted >= 0);
        } else {
            assert_eq!(item.time_deleted, -1);
        }
    }
    restore_all(items).unwrap();
    assert_eq!(fs::read_to_string(long).unwrap(), "long filename");
    assert_eq!(fs::read_to_string(binary).unwrap(), "native filename");
}

fn external_entries(work: &Path, trash: &Path, context: &TrashContext) {
    let seed = work.join("seed");
    fs::write(&seed, "seed").unwrap();
    context.delete(&seed).unwrap();
    restore_all(our_items(work)).unwrap();

    let invalid = "[Trash Info]\nPath=%QQ\nnot a key/value\n";
    let broken_info = trash.join("info/broken.trashinfo");
    let broken_payload = trash.join("files/broken");
    fs::write(&broken_info, invalid).unwrap();
    fs::write(&broken_payload, "unparseable receipt payload").unwrap();
    fs::write(trash.join("info/empty.trashinfo"), "").unwrap();
    let orphan = trash.join("files/collision");
    fs::write(&orphan, "untracked orphan").unwrap();
    let source = work.join("collision");
    fs::write(&source, "new payload").unwrap();
    context.delete(&source).unwrap();
    let items = our_items(work);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].original_path(), source);
    assert_ne!(payload(&items[0]), orphan);
    restore_all(items).unwrap();
    assert_eq!(fs::read_to_string(source).unwrap(), "new payload");
    assert_eq!(fs::read_to_string(orphan).unwrap(), "untracked orphan");
    assert_eq!(fs::read_to_string(broken_payload).unwrap(), "unparseable receipt payload");
    assert_eq!(fs::read_to_string(broken_info).unwrap(), invalid);
}
