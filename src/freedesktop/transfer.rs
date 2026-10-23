use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

/// Publish an item without overwriting a file, directory, or dangling symlink.
pub(super) fn rename_new(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{renameat_with, RenameFlags, CWD};
        renameat_with(CWD, source, CWD, target, RenameFlags::NOREPLACE).map_err(Into::into)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Reserve the name on platforms without Linux's atomic no-replace
        // operation. External writers replacing this reservation remain a race.
        if source.symlink_metadata()?.is_dir() {
            fs::DirBuilder::new().mode(0o700).create(target)?;
        } else {
            OpenOptions::new().create_new(true).write(true).mode(0o600).open(target)?;
        }
        let result = fs::rename(source, target);
        if result.is_err() {
            let _ = discard(target);
        }
        result
    }
}

pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Copy privately on the destination filesystem, publish the complete payload,
/// and commit its receipt before removing any source data. If source removal
/// fails, retain the published copy: recursive removal may already be partial.
pub(super) fn copy_then_remove(
    source: &Path,
    target: &Path,
    before_remove: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let parent =
        target.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"))?;
    let stage = tempfile::Builder::new().prefix(".trash-transfer-").tempdir_in(parent)?;
    let payload = stage.path().join("payload");
    let copied = copy_tree(source, &payload).and_then(|_| sync_directory(stage.path())).and_then(|_| {
        let metadata = payload.symlink_metadata()?;
        if metadata.is_dir() {
            // Reparenting a directory can require write access to update `..`.
            // Change only our private copy, then restore its mode before commit.
            let permissions = metadata.permissions();
            fs::set_permissions(&payload, fs::Permissions::from_mode(permissions.mode() | 0o700))?;
            Ok(Some(permissions))
        } else {
            Ok(None)
        }
    });
    let directory_permissions = match copied {
        Ok(permissions) => permissions,
        Err(error) => {
            let _ = discard(&payload);
            return Err(error);
        }
    };
    if let Err(error) = rename_new(&payload, target) {
        let _ = discard(&payload);
        return Err(io::Error::new(error.kind(), format!("cannot publish copied item at {target:?}: {error}")));
    }
    let ready = (|| {
        if let Some(permissions) = directory_permissions {
            let directory = File::open(target)?;
            directory.set_permissions(permissions)?;
            directory.sync_all()?;
        }
        sync_directory(parent)?;
        before_remove()
    })();
    if let Err(error) = ready {
        // Source data is still untouched. Take back only our published copy.
        let _ = discard(target);
        return Err(error);
    }
    remove(source).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("copy is complete at {target:?}, but source cleanup failed for {source:?}: {error}"),
        )
    })
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = source.symlink_metadata()?;
    if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
        return Ok(());
    }
    if metadata.is_dir() {
        fs::DirBuilder::new().mode(0o700).create(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        OpenOptions::new().create_new(true).write(true).mode(0o600).open(target)?;
        fs::copy(source, target)?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot copy special file {source:?} across filesystems; source left intact"),
        ));
    }
    let file = File::open(target)?;
    let mut times = fs::FileTimes::new();
    if let Ok(time) = metadata.accessed() {
        times = times.set_accessed(time);
    }
    if let Ok(time) = metadata.modified() {
        times = times.set_modified(time);
    }
    file.set_times(times)?;
    file.set_permissions(metadata.permissions())?;
    file.sync_all()
}

pub(super) fn remove(path: &Path) -> io::Result<()> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Remove only owned staging/copy data. Restoring a copied directory's original
/// mode can make it read-only, so temporarily add owner access during rollback.
fn discard(path: &Path) -> io::Result<()> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => {
            fs::set_permissions(path, fs::Permissions::from_mode(metadata.permissions().mode() | 0o700))?;
            for entry in fs::read_dir(path)? {
                discard(&entry?.path())?;
            }
            fs::remove_dir(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_refuses_existing_files_directories_and_dangling_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::write(&source, b"source").unwrap();
        for name in ["file", "directory", "symlink"] {
            let target = root.path().join(name);
            match name {
                "file" => fs::write(&target, b"existing").unwrap(),
                "directory" => fs::create_dir(&target).unwrap(),
                _ => std::os::unix::fs::symlink("missing", &target).unwrap(),
            }
            assert_eq!(rename_new(&source, &target).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(fs::read(&source).unwrap(), b"source");
            assert!(target.symlink_metadata().is_ok());
        }
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"existing");
        assert_eq!(fs::read_link(root.path().join("symlink")).unwrap(), Path::new("missing"));
    }

    #[test]
    fn failed_receipt_commit_takes_back_copy_and_keeps_source() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("data"), b"data").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o500)).unwrap();
        let result = copy_then_remove(&source, &target, || Err(io::Error::other("simulated receipt failure")));
        let error = result.unwrap_err();
        assert!(error.to_string().contains("simulated receipt failure"), "unexpected error: {error}");
        assert_eq!(fs::read(source.join("data")).unwrap(), b"data");
        assert!(!target.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn a_read_only_directory_is_published_with_its_original_mode() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o500)).unwrap();
        copy_then_remove(&source, &target, || {
            assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o500);
            Ok(())
        })
        .unwrap();
        assert!(!source.exists());
        assert!(target.is_dir());
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o500);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn failed_copy_leaves_no_partial_destination_or_staging_directory() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("data"), b"keep").unwrap();
        let socket = source.join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert_eq!(copy_then_remove(&source, &target, || Ok(())).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(fs::read(source.join("data")).unwrap(), b"keep");
        assert!(socket.exists());
        assert!(!target.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_publishers_preserve_the_losing_source() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        let target = root.path().join("target");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                rename_new(&a, &target)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                rename_new(&b, &target)
            });
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(usize::from(a.exists()) + usize::from(b.exists()), 1);
        let winner = fs::read(&target).unwrap();
        assert!(winner == b"a" || winner == b"b");
        if a.exists() {
            assert_eq!(fs::read(&a).unwrap(), b"a");
            assert_eq!(winner, b"b");
        } else {
            assert_eq!(fs::read(&b).unwrap(), b"b");
            assert_eq!(winner, b"a");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn source_cleanup_failure_keeps_the_complete_committed_copy() {
        // procfs provides readable data that cannot be unlinked, even by root.
        let source = Path::new("/proc/version");
        let expected = fs::read(source).unwrap();
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("backup");
        let mut committed = false;
        let error = copy_then_remove(source, &target, || {
            committed = true;
            Ok(())
        })
        .unwrap_err();
        assert!(committed);
        assert!(error.to_string().contains("source cleanup failed"));
        assert_eq!(fs::read(&target).unwrap(), expected);
        assert_eq!(fs::read(source).unwrap(), expected);
    }
}
