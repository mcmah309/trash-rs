//! This implementation will manage the trash according to the Freedesktop Trash specification,
//! version 1.0 found at <https://specifications.freedesktop.org/trash-spec/trashspec-1.0.html>
//!
//! Most -if not all- Linux based desktop operating systems implement the Trash according to this specification.
//! In other words: I looked, but I could not find any Linux based desktop OS that used anything else than the
//! Freedesktop Trash specification.
//!
//! Placement can be configured per [`TrashContext`] using
//! [`TrashContextExtFreedesktop`]. The default remains [`TrashLocation::PerFilesystem`].
//!
//! ```no_run
//! # #[cfg(all(unix, not(target_os = "macos"), not(target_os = "ios"), not(target_os = "android"), feature = "freedesktop"))]
//! # fn example() -> Result<(), trash::Error> {
//! use trash::{TrashContext, freedesktop::{TrashContextExtFreedesktop, TrashLocation}};
//!
//! let mut context = TrashContext::new();
//! context.set_trash_location(TrashLocation::Home);
//! context.delete("report.txt")?;
//! # Ok(())
//! # }
//! ```
//!
//! Cross-filesystem transfers preserve regular file and directory permissions
//! and timestamps, and copy symbolic links without following them. Ownership,
//! hard-link relationships, ACLs, and extended attributes are not preserved.
//! Unsupported special files fail without removing the source. Copy failures
//! clean up partial destinations; if removing a source fails after a complete
//! copy, the complete destination is retained and an error is returned.
//! Interrupted copies may leave a private staging directory with an intact source.

mod transfer;

use std::{
    borrow::Borrow,
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};

use log::{debug, warn};

use crate::{Error, TrashContext, TrashItem, TrashItemMetadata, TrashItemSize};

type FsError = (PathBuf, std::io::Error);

/// Placement policy for new deletions with the FreeDesktop backend.
///
/// This does not restrict listing, restoration, or purging of existing entries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum TrashLocation {
    /// Prefer trash on the source filesystem, falling back to home trash when
    /// per-filesystem trash cannot be created due to permissions. The default.
    #[default]
    PerFilesystem,
    /// Always use `$XDG_DATA_HOME/Trash` (or `$HOME/.local/share/Trash`).
    ///
    /// Files on other filesystems are copied before their sources are removed.
    /// This requires destination space and can take longer than a rename. An
    /// unusable home trash is an error; no per-filesystem fallback is attempted.
    Home,
}

/// FreeDesktop-specific preferences for a [`TrashContext`].
pub trait TrashContextExtFreedesktop {
    /// Set placement for subsequent deletions on this context.
    fn set_trash_location(&mut self, location: TrashLocation);
    /// Return the placement policy used by this context.
    fn trash_location(&self) -> TrashLocation;
}

impl TrashContextExtFreedesktop for TrashContext {
    fn set_trash_location(&mut self, location: TrashLocation) {
        self.platform_specific.location = location;
    }

    fn trash_location(&self) -> TrashLocation {
        self.platform_specific.location
    }
}

#[derive(Clone, Default, Debug)]
pub(crate) struct PlatformTrashContext {
    location: TrashLocation,
}
impl PlatformTrashContext {
    pub const fn new() -> Self {
        Self { location: TrashLocation::PerFilesystem }
    }
}
impl TrashContext {
    pub(crate) fn delete_all_canonicalized(&self, full_paths: Vec<PathBuf>) -> Result<(), Error> {
        let home_trash = canonicalize_path_or_parents(home_trash()?.as_path())?;
        let sorted_mount_points = get_sorted_mount_points()?;
        // TargetedRoot promises that no operand in this batch was removed.
        if full_paths.iter().any(|path| sorted_mount_points.iter().any(|mount| path == &mount.mnt_dir)) {
            return Err(Error::TargetedRoot);
        }
        let home_trash_topdir = get_first_topdir_containing_path(&home_trash, &sorted_mount_points);
        debug!("The 'home trash' topdir is {:?}", home_trash_topdir);
        let uid = unsafe { libc::getuid() };
        for path in full_paths {
            debug!("Deleting {:?}", path);
            let topdir = get_first_topdir_containing_path(&path, &sorted_mount_points);
            debug!("The topdir of this file is {:?}", topdir);
            // Never move trash into itself or into one of its descendants.
            for root in std::iter::once(home_trash.clone()).chain(sorted_mount_points.iter().flat_map(|mount| {
                [mount.mnt_dir.join(format!(".Trash-{uid}")), mount.mnt_dir.join(".Trash").join(uid.to_string())]
            })) {
                if path.starts_with(&root) || (path.is_dir() && root.starts_with(&path)) {
                    return Err(fs_error(
                        &path,
                        io::Error::other("cannot trash trash storage or a directory containing it"),
                    ));
                }
            }
            if self.trash_location() == TrashLocation::Home || topdir == home_trash_topdir {
                debug!("Using home trash due to the context policy or source filesystem.");
                // Note that the following function creates the trash folder
                // and its required subfolders in case they don't exist.
                move_to_trash(path, &home_trash, None::<&Path>).map_err(|(p, e)| fs_error(p, e))?;
            } else {
                // Try the per-volume trash first ($topdir/.Trash/$uid or
                // $topdir/.Trash-$uid).  When that fails because the
                // mount-point root is not writable, fall back to the home
                // trash - the spec says an implementation "MUST either
                // trash the file into the user's home trash or refuse to
                // trash it" when per-volume trash is unavailable.
                let mut selected = None;
                let selection = execute_on_mounted_trash_folders(uid, topdir, true, true, |trash_path| {
                    prepare_trash_directory(&trash_path)?;
                    selected = Some(trash_path);
                    Ok(())
                });
                match selection {
                    Ok(()) => {
                        let root = selected.ok_or_else(|| Error::Unknown {
                            description: "no usable per-filesystem trash directory".into(),
                        })?;
                        // Once transfer starts, never retry in another trash: a
                        // cleanup error can leave a complete backup and a partial source.
                        move_to_trash(path, root, Some(topdir)).map_err(|(p, e)| fs_error(p, e))?;
                    }
                    Err((_, ref e)) if e.kind() == ErrorKind::PermissionDenied => {
                        debug!("Per-volume trash on {:?} failed ({}), falling back to home trash.", topdir, e);
                        move_to_trash(path, &home_trash, None::<&Path>).map_err(|(p, e)| fs_error(p, e))?;
                    }
                    Err((p, e)) => return Err(fs_error(p, e)),
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn list() -> Result<Vec<TrashItem>, Error> {
    let EvaluatedTrashFolders { trash_folders, sorted_mount_points, .. } = eval_trash_folders()?;
    let home = home_trash().and_then(|path| canonicalize_path_or_parents(&path)).ok();
    let mut result = Vec::new();
    for folder in trash_folders {
        let base = if home.as_ref() == Some(&folder) {
            folder.parent().unwrap_or(Path::new("/"))
        } else {
            get_first_topdir_containing_path(&folder, &sorted_mount_points)
        };
        let info_folder = folder.join("info");
        if !checked_directory(&info_folder, false).map_err(|e| fs_error(&info_folder, e))? {
            continue;
        }
        for entry in fs::read_dir(&info_folder).map_err(|e| fs_error(&info_folder, e))? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if missing(&error) => continue,
                Err(error) => return Err(fs_error(&info_folder, error)),
            };
            let path = entry.path();
            if path.extension() != Some(OsStr::new("trashinfo")) {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_file() => {}
                Ok(_) => continue,
                Err(error) if missing(&error) => continue,
                Err(error) => return Err(fs_error(&path, error)),
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if missing(&error) => continue,
                Err(error) => return Err(fs_error(&path, error)),
            };
            // Empty receipts reserve a name while a cross-device copy is staged.
            if bytes.is_empty() {
                continue;
            }
            match parse_receipt(&path, &bytes, base) {
                Ok(item) => result.push(item),
                Err(error) => warn!("skipping malformed trash receipt {path:?}: {error}"),
            }
        }
    }
    Ok(result)
}

fn parse_receipt(id: &Path, bytes: &[u8], base: &Path) -> io::Result<TrashItem> {
    let invalid = || io::Error::new(ErrorKind::InvalidData, "invalid trash metadata");
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    let mut lines = text.lines();
    if lines.next() != Some("[Trash Info]") {
        return Err(invalid());
    }
    let mut original = None;
    #[cfg_attr(not(feature = "chrono"), allow(unused_variables))]
    let mut deleted = None;
    for line in lines {
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            "Path" if original.is_none() => {
                let encoded = value.as_bytes();
                let mut offset = 0;
                while offset < encoded.len() {
                    if encoded[offset] == b'%' {
                        if !encoded.get(offset + 1..offset + 3).is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
                        {
                            return Err(invalid());
                        }
                        offset += 3;
                    } else {
                        offset += 1;
                    }
                }
                let path = decode_uri_path(Path::new(value));
                if path.as_os_str().is_empty()
                    || path.as_os_str().as_bytes().contains(&0)
                    || path.components().any(|part| matches!(part, Component::ParentDir))
                {
                    return Err(invalid());
                }
                original = Some(if path.is_absolute() { path } else { base.join(path) });
            }
            "DeletionDate" if deleted.is_none() => deleted = Some(value),
            _ => {}
        }
    }
    let original = original.ok_or_else(invalid)?;
    let name = original.file_name().ok_or_else(invalid)?.to_os_string();
    let original_parent = original.parent().ok_or_else(invalid)?.to_path_buf();
    #[cfg(feature = "chrono")]
    let time_deleted = {
        use chrono::{NaiveDateTime, TimeZone};
        deleted
            .and_then(|text| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").ok())
            .and_then(|date| chrono::Local.from_local_datetime(&date).earliest())
            .map(|date| date.timestamp())
            .unwrap_or(-1)
    };
    #[cfg(not(feature = "chrono"))]
    let time_deleted = -1;
    Ok(TrashItem { id: id.as_os_str().to_os_string(), name, original_parent, time_deleted })
}

pub(crate) fn is_empty() -> Result<bool, Error> {
    let trash_folders = trash_folders()?;

    if trash_folders.is_empty() {
        return Ok(true);
    }

    for folder in trash_folders {
        // We're only concerned if the trash contains any files
        // Therefore, we only need to check if the bin itself is empty
        let bin = folder.join("files");
        match bin.read_dir() {
            Ok(mut entries) => {
                if let Some(Ok(_)) = entries.next() {
                    return Ok(false);
                }
            }
            Err(e) => {
                warn!("The trash files folder {:?} could not be read. Error was {:?}", bin, e);
            }
        }
    }

    Ok(true)
}

pub(crate) fn trash_folders() -> Result<HashSet<PathBuf>, Error> {
    let EvaluatedTrashFolders { trash_folders, home_error, .. } = eval_trash_folders()?;

    if trash_folders.is_empty() {
        return match home_error {
            Some(e) => Err(e),
            None => Err(Error::Unknown {
                description: "Could not find a valid 'home trash' nor valid trashes on other mount points".into(),
            }),
        };
    }

    Ok(trash_folders)
}

struct EvaluatedTrashFolders {
    trash_folders: HashSet<PathBuf>,
    home_error: Option<Error>,
    sorted_mount_points: Vec<MountPoint>,
}

fn eval_trash_folders() -> Result<EvaluatedTrashFolders, Error> {
    let mut trash_folders = HashSet::new();
    // Get home trash folder and add it to the set of trash folders.
    // It may not exist and that's completely fine as long as there are other trash folders.
    let home_error;
    match home_trash().and_then(|path| canonicalize_path_or_parents(&path)) {
        Ok(home_trash) => {
            if !home_trash.is_dir() {
                home_error = Some(Error::Unknown {
                    description:
                        "The 'home trash' either does not exist or is not a directory (or a link pointing to a dir)"
                            .into(),
                });
            } else {
                trash_folders.insert(home_trash);
                home_error = None;
            }
        }
        Err(e) => {
            home_error = Some(e);
        }
    }

    // Get all mount-points and attempt to find a trash folder in each adding them to the SET of
    // trash folders when found one.
    let uid = unsafe { libc::getuid() };
    let sorted_mount_points = get_sorted_mount_points()?;
    for mount in &sorted_mount_points {
        execute_on_mounted_trash_folders(uid, &mount.mnt_dir, false, false, |trash_path| {
            trash_folders.insert(trash_path);
            Ok(())
        })
        .map_err(|(p, e)| fs_error(p, e))?;
    }

    Ok(EvaluatedTrashFolders { trash_folders, home_error, sorted_mount_points })
}
pub(crate) fn metadata(item: &TrashItem) -> Result<TrashItemMetadata, Error> {
    // When purging an item the "in-trash" filename must be parsed from the trashinfo filename
    // which is the filename in the `id` field.
    let info_file = &item.id;

    let file = restorable_file_in_trash_from_info_file(info_file);
    ensure_virtually_exists(&file)?;
    let metadata = fs::symlink_metadata(&file).map_err(|e| fs_error(&file, e))?;
    let is_dir = metadata.is_dir();
    let size = if is_dir {
        TrashItemSize::Entries(fs::read_dir(&file).map_err(|e| fs_error(&file, e))?.count())
    } else {
        TrashItemSize::Bytes(metadata.len())
    };
    Ok(TrashItemMetadata { size })
}

/// The path points to:
/// - existing file | directory | symlink => Ok(true)
/// - broken symlink => Ok(true)
/// - nothing => Ok(false)
/// - I/O Error => Err(Io)
#[inline]
fn virtually_exists(path: &Path) -> std::io::Result<bool> {
    Ok(path.try_exists()? || path.is_symlink())
}

const MISSING_TRASH_FILE: &str = "trash item referenced by .trashinfo is missing from trash/files";

/// Ensures the path virtually exists, returning an error if it does not.
/// This is used instead of `assert!` to gracefully handle cases where the trash file
/// referenced by a `.trashinfo` file is missing from the `files` directory.
#[inline]
fn ensure_virtually_exists(path: &Path) -> Result<(), Error> {
    if virtually_exists(path).map_err(|e| fs_error(path, e))? {
        Ok(())
    } else {
        Err(fs_error(path, std::io::Error::new(std::io::ErrorKind::NotFound, MISSING_TRASH_FILE)))
    }
}

pub(crate) fn purge_all<I>(items: I) -> Result<(), Error>
where
    I: IntoIterator,
    <I as IntoIterator>::Item: Borrow<TrashItem>,
{
    for item in items.into_iter() {
        // When purging an item the "in-trash" filename must be parsed from the trashinfo filename
        // which is the filename in the `id` field.
        let info_file = &item.borrow().id;

        // A bunch of unwraps here. This is fine because if any of these fail that means
        // that either there's a bug in this code or the target system didn't follow
        // the specification.
        let file = restorable_file_in_trash_from_info_file(info_file);
        if file.is_dir() {
            std::fs::remove_dir_all(&file).map_err(|e| fs_error(&file, e))?;
        // TODO Update directory size cache if there's one.
        } else {
            // Ignore the error if the file is already missing.
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(fs_error(&file, e)),
            }
        }
        std::fs::remove_file(info_file).map_err(|e| fs_error(info_file, e))?;
    }

    Ok(())
}

fn restorable_file_in_trash_from_info_file(info_file: impl AsRef<std::ffi::OsStr>) -> PathBuf {
    let info_file = info_file.as_ref();
    let trash_folder = Path::new(info_file).parent().unwrap().parent().unwrap();
    let name_in_trash = Path::new(info_file).file_stem().unwrap();
    trash_folder.join("files").join(name_in_trash)
}

pub(crate) fn restore_all<I>(items: I) -> Result<(), Error>
where
    I: IntoIterator<Item = TrashItem>,
{
    let mut iter = items.into_iter();
    while let Some(item) = iter.next() {
        let payload = restorable_file_in_trash_from_info_file(&item.id);
        ensure_virtually_exists(&payload)?;
        let target = item.original_path();
        fs::create_dir_all(&item.original_parent).map_err(|e| fs_error(&item.original_parent, e))?;
        let moved = match transfer::rename_new(&payload, &target) {
            Ok(()) => transfer::sync_directory(&item.original_parent),
            Err(error) if error.kind() == ErrorKind::CrossesDevices => {
                transfer::copy_then_remove(&payload, &target, || Ok(()))
            }
            Err(error) => Err(error),
        };
        match moved {
            Ok(()) => remove_receipt(Path::new(&item.id)).map_err(|e| fs_error(&item.id, e))?,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Err(Error::RestoreCollision {
                    path: target,
                    remaining_items: std::iter::once(item).chain(iter).collect(),
                });
            }
            Err(error) => return Err(fs_error(&payload, error)),
        }
    }
    Ok(())
}

/// According to the specification (see at the top of the file) there are two kinds of
/// trash-folders for a mounted drive or partition.
/// 1, .Trash/uid
/// 2, .Trash-uid
///
/// This function executes `op` providing it with a
/// trash-folder path that's associated with the partition mounted at `topdir`.
///
fn execute_on_mounted_trash_folders<F: FnMut(PathBuf) -> Result<(), FsError>>(
    uid: u32,
    topdir: impl AsRef<Path>,
    first_only: bool,
    create_folder: bool,
    mut op: F,
) -> Result<(), FsError> {
    let topdir = topdir.as_ref();
    let shared = topdir.join(".Trash");
    match shared.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() && folder_validity(&shared)? == TrashValidity::Valid => {
            let root = shared.join(uid.to_string());
            match checked_directory(&root, create_folder) {
                Ok(true) => {
                    op(root)?;
                    if first_only {
                        return Ok(());
                    }
                }
                Ok(false) => {}
                Err(error) if create_folder && error.kind() == ErrorKind::PermissionDenied => {}
                Err(error) => return Err((root, error)),
            }
        }
        Ok(_) => {}
        Err(error) if missing(&error) => {}
        // Virtual/protected mount namespaces need not be readable for discovery.
        Err(error) if !create_folder && error.kind() == ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err((shared, error)),
    }
    let root = topdir.join(format!(".Trash-{uid}"));
    match checked_directory(&root, create_folder) {
        Ok(true) => op(root)?,
        Ok(false) => {}
        Err(error) if !create_folder && error.kind() == ErrorKind::PermissionDenied => {}
        Err(error) => return Err((root, error)),
    }
    Ok(())
}

fn missing(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

fn checked_directory(path: &Path, create: bool) -> io::Result<bool> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if missing(&error) && !create => return Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound && create => {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
            path.symlink_metadata()?
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::other(format!(
            "trash directory {path:?} must be a real directory owned by the current user"
        )));
    }
    Ok(true)
}

struct ReceiptReservation {
    path: PathBuf,
    keep: bool,
}

impl Drop for ReceiptReservation {
    fn drop(&mut self) {
        if !self.keep {
            let _ = remove_receipt(&self.path);
        }
    }
}

fn write_receipt(path: &Path, original: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("receipt has no parent"))?;
    let mut temp = tempfile::Builder::new().prefix(".trash-info-").tempfile_in(parent)?;
    writeln!(temp, "[Trash Info]\nPath={}", encode_uri_path(original))?;
    #[cfg(feature = "chrono")]
    writeln!(temp, "DeletionDate={}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"))?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    transfer::sync_directory(parent)
}

fn remove_receipt(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if missing(&error) => {}
        Err(error) => return Err(error),
    }
    transfer::sync_directory(path.parent().ok_or_else(|| io::Error::other("receipt has no parent"))?)
}

fn storage_name(original: &OsStr, index: u64) -> OsString {
    let mut name = if original.as_bytes().len() > 220 {
        // Receipt suffixes and collision counters must also fit NAME_MAX.
        OsString::from(format!("trash-{}", std::process::id()))
    } else {
        original.to_os_string()
    };
    if index > 0 {
        name.push(format!(".{}", index + 1));
    }
    name
}

fn prepare_trash_directory(root: &Path) -> Result<(), FsError> {
    for directory in [root.to_path_buf(), root.join("files"), root.join("info")] {
        checked_directory(&directory, true).map_err(|error| (directory, error))?;
    }
    Ok(())
}

fn move_to_trash(
    src: impl AsRef<Path>,
    trash_folder: impl AsRef<Path>,
    topdir: Option<impl AsRef<Path>>,
) -> Result<(), FsError> {
    let src = src.as_ref();
    let trash_folder = trash_folder.as_ref();
    let files_folder = trash_folder.join("files");
    let info_folder = trash_folder.join("info");
    prepare_trash_directory(trash_folder)?;
    let original = match topdir {
        Some(topdir) => src.strip_prefix(topdir.as_ref()).map_err(|e| (src.to_path_buf(), io::Error::other(e)))?,
        None => src,
    };
    let filename = src.file_name().ok_or_else(|| (src.to_path_buf(), io::Error::other("source has no filename")))?;
    for index in 0_u64.. {
        let name = storage_name(filename, index);
        let target = files_folder.join(&name);
        let mut info_name = name;
        info_name.push(".trashinfo");
        let info = info_folder.join(info_name);
        match OpenOptions::new().create_new(true).write(true).mode(0o600).open(&info) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err((info, error)),
        }
        let mut receipt = ReceiptReservation { path: info.clone(), keep: false };
        match target.symlink_metadata() {
            Ok(_) => continue,
            Err(error) if missing(&error) => {}
            Err(error) => return Err((target, error)),
        }
        write_receipt(&info, original).map_err(|e| (info.clone(), e))?;
        match transfer::rename_new(src, &target) {
            Ok(()) => {
                receipt.keep = true;
                return transfer::sync_directory(&files_folder).map_err(|e| (files_folder, e));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == ErrorKind::CrossesDevices => {
                let file = OpenOptions::new().write(true).truncate(true).open(&info).map_err(|e| (info.clone(), e))?;
                file.sync_all().map_err(|e| (info.clone(), e))?;
                return transfer::copy_then_remove(src, &target, || {
                    write_receipt(&info, original)?;
                    receipt.keep = true;
                    Ok(())
                })
                .map_err(|e| (src.to_path_buf(), e));
            }
            Err(error) => return Err((src.to_path_buf(), error)),
        }
    }
    unreachable!("all possible trash filenames exhausted")
}

fn decode_uri_path(path: impl AsRef<Path>) -> PathBuf {
    // Decode the complete byte string: pushing separately decoded components
    // would let an encoded '/' reset the path accumulated so far.
    PathBuf::from(OsString::from_vec(urlencoding::decode_binary(path.as_ref().as_os_str().as_bytes()).to_vec()))
}

fn encode_uri_path(path: impl AsRef<Path>) -> String {
    // `iter()` cannot be used here because it yields '/' in certain situations, such as
    // for root directories.
    // Slashes would be encoded and thus mess up the path
    let path: PathBuf = path
        .as_ref()
        .components()
        .map(|component| {
            // Only encode names and not '/', 'C:\', et cetera
            if let Component::Normal(part) = component {
                urlencoding::encode_binary(part.as_bytes()).to_string()
            } else {
                component.as_os_str().to_str().expect("Path components such as '/' are valid Unicode").to_owned()
            }
        })
        .collect();

    path.to_str().expect("URL encoded bytes is valid Unicode").to_owned()
}

#[derive(Eq, PartialEq, Debug)]
enum TrashValidity {
    Valid,
    InvalidSymlink,
    InvalidNotSticky,
}

fn folder_validity(path: impl AsRef<Path>) -> Result<TrashValidity, FsError> {
    /// Mask for the sticky bit
    /// Taken from: http://man7.org/linux/man-pages/man7/inode.7.html
    const S_ISVTX: u32 = 0o1000;

    let path = path.as_ref();
    let metadata = path.symlink_metadata().map_err(|e| (path.to_owned(), e))?;
    if metadata.file_type().is_symlink() {
        return Ok(TrashValidity::InvalidSymlink);
    }
    let mode = metadata.permissions().mode();
    let no_sticky_bit = (mode & S_ISVTX) == 0;
    if no_sticky_bit {
        return Ok(TrashValidity::InvalidNotSticky);
    }
    Ok(TrashValidity::Valid)
}

/// Corresponds to the definition of "home_trash" from
/// https://specifications.freedesktop.org/trash-spec/trashspec-1.0.html
fn home_trash() -> Result<PathBuf, Error> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        if !data_home.is_empty() {
            let data_home_path = AsRef::<Path>::as_ref(data_home.as_os_str());
            return Ok(data_home_path.join("Trash"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            let home_path = AsRef::<Path>::as_ref(home.as_os_str());
            return Ok(home_path.join(".local/share/Trash"));
        }
    }
    Err(Error::Unknown { description: "Neither the XDG_DATA_HOME nor the HOME environment variable was found".into() })
}

fn get_first_topdir_containing_path<'a>(path: &Path, mnt_points: &'a [MountPoint]) -> &'a Path {
    let root: &'static Path = Path::new("/");
    mnt_points.iter().map(|mp| mp.mnt_dir.as_path()).find(|mount_path| path.starts_with(mount_path)).unwrap_or(root)
}

/// Canonicalize a path. If the path doesn't exist, the canonical form of the
/// path is determined by looking at its parent directories.
fn canonicalize_path_or_parents(mut path: &Path) -> Result<PathBuf, Error> {
    let mut popped_path_components = vec![];

    loop {
        match path.canonicalize() {
            Ok(canonical) => {
                return Ok(popped_path_components.iter().rev().fold(canonical, |acc, component| acc.join(component)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let file_name = path.file_name().ok_or(Error::CanonicalizePath { original: path.to_owned() })?;
                popped_path_components.push(file_name.to_owned());
                path = path.parent().ok_or(Error::CanonicalizePath { original: path.to_owned() })?;
            }
            Err(e) => return Err(Error::FileSystem { path: path.to_owned(), source: e }),
        }
    }
}

struct MountPoint {
    mnt_dir: PathBuf,
    _mnt_type: String,
    _mnt_fsname: String,
}

/// Sorted by longest path first
fn get_sorted_mount_points() -> Result<Vec<MountPoint>, Error> {
    let mut mount_points = get_mount_points()?;
    mount_points.sort_unstable_by(|a, b| {
        let a = a.mnt_dir.as_os_str().as_bytes().len();
        let b = b.mnt_dir.as_os_str().as_bytes().len();
        a.cmp(&b).reverse()
    });
    Ok(mount_points)
}

#[cfg(target_os = "linux")]
fn get_mount_points() -> Result<Vec<MountPoint>, Error> {
    let path = Path::new("/proc/self/mountinfo");
    let bytes = fs::read(path).map_err(|e| fs_error(path, e))?;
    parse_mountinfo(&bytes).map_err(|e| fs_error(path, e))
}

#[cfg(target_os = "linux")]
fn parse_mountinfo(bytes: &[u8]) -> io::Result<Vec<MountPoint>> {
    let invalid = || io::Error::new(ErrorKind::InvalidData, "invalid mountinfo entry");
    let mut mounts = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split(|byte| *byte == b' ').collect();
        let field = *fields.get(4).ok_or_else(invalid)?;
        let separator = fields.iter().position(|field| *field == b"-").ok_or_else(invalid)?;
        let kind = *fields.get(separator + 1).ok_or_else(invalid)?;
        let mut decoded = Vec::new();
        let mut offset = 0;
        while offset < field.len() {
            if field[offset] == b'\\' {
                let digits = field.get(offset + 1..offset + 4).ok_or_else(invalid)?;
                if !digits.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
                    return Err(invalid());
                }
                let value = (digits[0] - b'0') as u16 * 64 + (digits[1] - b'0') as u16 * 8 + (digits[2] - b'0') as u16;
                decoded.push(u8::try_from(value).map_err(|_| invalid())?);
                offset += 4;
            } else {
                decoded.push(field[offset]);
                offset += 1;
            }
        }
        let mnt_dir = PathBuf::from(OsString::from_vec(decoded));
        if !mnt_dir.is_absolute() {
            return Err(invalid());
        }
        mounts.push(MountPoint {
            mnt_dir,
            _mnt_type: String::from_utf8_lossy(kind).into_owned(),
            _mnt_fsname: String::new(),
        });
    }
    if mounts.is_empty() {
        return Err(invalid());
    }
    Ok(mounts)
}

#[cfg(any(target_os = "dragonfly", target_os = "freebsd", target_os = "openbsd"))]
fn get_mount_points() -> Result<Vec<MountPoint>, Error> {
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    // The getmntinfo() function writes the array of structures to an internal
    // static object and returns a pointer to that object.  Subsequent calls to
    // getmntinfo() will modify the same object. This means that the function is
    // not threadsafe. To help prevent multiple threads using it concurrently
    // via get_mount_points a Mutex is used.
    // We understand that threads can still call `libc::getmntinfo(…)` directly
    // to bypass the lock and trigger UB.
    static LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
    let _lock = LOCK.lock().unwrap();

    fn c_buf_to_str(buf: &[libc::c_char]) -> Option<&str> {
        let buf: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as _, buf.len()) };
        if let Some(pos) = buf.iter().position(|x| *x == 0) {
            // Shrink buffer to omit the null bytes
            std::str::from_utf8(&buf[..pos]).ok()
        } else {
            std::str::from_utf8(buf).ok()
        }
    }
    let mut fs_infos: *mut libc::statfs = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut fs_infos, libc::MNT_WAIT) };
    if count < 1 {
        return Ok(Vec::new());
    }
    let fs_infos: &[libc::statfs] = unsafe { std::slice::from_raw_parts(fs_infos as _, count as _) };

    let mut result = Vec::new();
    for fs_info in fs_infos {
        if fs_info.f_mntfromname[0] == 0 || fs_info.f_mntonname[0] == 0 {
            // If we have missing information, no need to look any further...
            continue;
        }
        let fs_type = c_buf_to_str(&fs_info.f_fstypename).unwrap_or_default();
        let mount_to = match c_buf_to_str(&fs_info.f_mntonname) {
            Some(m) => m,
            None => {
                debug!("Cannot get disk mount point, ignoring it.");
                continue;
            }
        };
        let mount_from = c_buf_to_str(&fs_info.f_mntfromname).unwrap_or_default();

        let mount_point =
            MountPoint { mnt_dir: mount_to.into(), _mnt_fsname: mount_from.into(), _mnt_type: fs_type.into() };
        result.push(mount_point);
    }
    Ok(result)
}

#[cfg(target_os = "netbsd")]
fn get_mount_points() -> Result<Vec<MountPoint>, Error> {
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    // The getmntinfo() function writes the array of structures to an internal
    // static object and returns a pointer to that object.  Subsequent calls to
    // getmntinfo() will modify the same object. This means that the function is
    // not threadsafe. To help prevent multiple threads using it concurrently
    // via get_mount_points a Mutex is used.
    // We understand that threads can still call `libc::getmntinfo(…)` directly
    // to bypass the lock and trigger UB.
    // NetBSD does not support statfs since 2005, so we need to use statvfs instead.
    static LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
    let _lock = LOCK.lock().unwrap();

    fn c_buf_to_str(buf: &[libc::c_char]) -> Option<&str> {
        let buf: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as _, buf.len()) };
        if let Some(pos) = buf.iter().position(|x| *x == 0) {
            // Shrink buffer to omit the null bytes
            std::str::from_utf8(&buf[..pos]).ok()
        } else {
            std::str::from_utf8(buf).ok()
        }
    }
    let mut fs_infos: *mut libc::statvfs = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut fs_infos, libc::MNT_WAIT) };
    if count < 1 {
        return Ok(Vec::new());
    }
    let fs_infos: &[libc::statvfs] = unsafe { std::slice::from_raw_parts(fs_infos as _, count as _) };

    let mut result = Vec::new();
    for fs_info in fs_infos {
        if fs_info.f_mntfromname[0] == 0 || fs_info.f_mntonname[0] == 0 {
            // If we have missing information, no need to look any further...
            continue;
        }
        let fs_type = c_buf_to_str(&fs_info.f_fstypename).unwrap_or_default();
        let mount_to = match c_buf_to_str(&fs_info.f_mntonname) {
            Some(m) => m,
            None => {
                debug!("Cannot get disk mount point, ignoring it.");
                continue;
            }
        };
        let mount_from = c_buf_to_str(&fs_info.f_mntfromname).unwrap_or_default();

        let mount_point =
            MountPoint { mnt_dir: mount_to.into(), _mnt_fsname: mount_from.into(), _mnt_type: fs_type.into() };
        result.push(mount_point);
    }
    Ok(result)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
fn get_mount_points() -> Result<Vec<MountPoint>, Error> {
    // On platforms that don't have support yet, return an error
    Err(Error::Unknown { description: "Mount points cannot be determined on this operating system".into() })
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use std::{
        collections::{hash_map::Entry, HashMap},
        env,
        ffi::{OsStr, OsString},
        fmt,
        fs::File,
        io::ErrorKind,
        os::unix::{self, ffi::OsStringExt, fs::PermissionsExt},
        path::{Path, PathBuf},
        process::Command,
    };

    use log::warn;

    use crate::{
        canonicalize_paths, delete, delete_all,
        os_limited::{list, purge_all, restore_all},
        platform::encode_uri_path,
        tests::get_unique_name,
        Error,
    };

    use super::{canonicalize_path_or_parents, decode_uri_path};

    #[test]
    fn receipt_parsing_is_lossless_and_rejects_unsafe_or_malformed_paths() {
        let id = Path::new("/trash/info/entry.trashinfo");
        let item = super::parse_receipt(
            id,
            b"[Trash Info]\n\n# comment\nUnknown=ignored\nPath=dir/a%3Db%20%FF\nPath=ignored\n",
            Path::new("/mount"),
        )
        .unwrap();
        assert_eq!(item.original_path(), PathBuf::from(OsString::from_vec(b"/mount/dir/a=b \xff".to_vec())));
        let item = super::parse_receipt(id, b"[Trash Info]\nPath=dir/%2Ffile\n", Path::new("/mount")).unwrap();
        assert_eq!(item.original_path(), Path::new("/mount/dir/file"));
        for bytes in [
            b"wrong header".as_slice(),
            b"[Trash Info]\nPath=%QQ\n",
            b"[Trash Info]\nPath=../escape\n",
            b"[Trash Info]\nPath=%2E%2E/escape\n",
            b"[Trash Info]\nPath=%00\n",
            b"[Trash Info]\nPath=\n",
            b"[Trash Info]\nPath=/\n",
        ] {
            assert!(super::parse_receipt(id, bytes, Path::new("/mount")).is_err(), "{bytes:?}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mountinfo_preserves_bind_boundaries_and_escaped_native_names() {
        let mounts = super::parse_mountinfo(
            b"1 0 8:1 / / rw - ext4 /dev/root rw\n\
              2 1 8:1 /source /workspace\\040name rw - ext4 /dev/root rw\n\
              3 1 8:1 /other /binary\\377 rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        assert_eq!(mounts[1].mnt_dir, Path::new("/workspace name"));
        assert_eq!(mounts[2].mnt_dir, PathBuf::from(OsString::from_vec(b"/binary\xff".to_vec())));
        assert_eq!(
            super::get_first_topdir_containing_path(Path::new("/workspace name/file"), &mounts[1..]),
            Path::new("/workspace name")
        );
        for bytes in [b"".as_slice(), b"malformed", b"1 0 8:1 / /bad\\999 rw - ext4 root rw\n"] {
            assert!(super::parse_mountinfo(bytes).is_err());
        }
    }

    #[test]
    #[serial]
    fn test_list() {
        crate::tests::init_logging();

        let file_name_prefix = get_unique_name();
        let batches: usize = 2;
        let files_per_batch: usize = 3;
        let names: Vec<OsString> = (0..files_per_batch).map(|i| format!("{}#{}", file_name_prefix, i).into()).collect();
        for _ in 0..batches {
            for path in &names {
                File::create_new(path).unwrap();
            }
            // eprintln!("Deleting {:?}", names);
            let result = delete_all_using_system_program(&names);
            if let Err(SystemTrashError::NoTrashProgram) = &result {
                // For example may be the case on build systems that don't have a destop environment
                warn!("No system default trashing utility was found, using this crate's implementation");
                delete_all(&names).unwrap();
            } else {
                result.unwrap();
            }
        }
        let items = list().unwrap();
        let items: HashMap<_, Vec<_>> = items
            .into_iter()
            .filter(|x| x.name.as_encoded_bytes().starts_with(file_name_prefix.as_bytes()))
            .fold(HashMap::new(), |mut map, x| {
                match map.entry(x.name.clone()) {
                    Entry::Occupied(mut entry) => {
                        entry.get_mut().push(x);
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(vec![x]);
                    }
                }
                map
            });
        for name in names {
            match items.get(&name) {
                Some(items) => assert_eq!(items.len(), batches),
                None => panic!("ERROR Could not find '{:?}' in {:#?}", name, items),
            }
        }

        // Let's try to purge all the items we just created but ignore any errors
        // as this test should succeed as long as `list` works properly.
        let _ = purge_all(items.into_values().flatten());
    }

    #[test]
    #[serial]
    fn test_broken_symlinks() {
        crate::tests::init_logging();

        let file_name_prefix = get_unique_name();
        let file_count = 2;
        let names: Vec<OsString> = (0..file_count).map(|i| format!("{}#{}", file_name_prefix, i).into()).collect();
        let symlink_names: Vec<OsString> = names.iter().map(|name| format!("{:?}-symlink", name).into()).collect();

        // Test file symbolic link and directory symbolic link
        File::create_new(&names[0]).unwrap();
        std::fs::create_dir(&names[1]).unwrap();

        for (i, (name, symlink)) in names.iter().zip(&symlink_names).enumerate() {
            // Create symbolic link
            unix::fs::symlink(name, symlink).unwrap();

            // Break the symbolic link
            if i == 0 {
                std::fs::remove_file(name).unwrap();
            } else {
                std::fs::remove_dir(name).unwrap();
            }

            // Delete and Restore it without errors
            delete(symlink).unwrap();
            let items = list().unwrap();
            let item = items.into_iter().find(|it| it.name == *symlink).unwrap();
            restore_all([item.clone()]).expect("The broken symbolic link should be restored successfully.");

            // Delete and Purge it without errors
            delete(symlink).unwrap();
            purge_all([item]).expect("The broken symbolic link should be purged successfully.");
        }
    }

    #[test]
    fn uri_enc_dec_roundtrip() {
        let fake = format!("/tmp/{}", get_unique_name());
        let path = decode_uri_path(&fake);

        assert_eq!(path.to_str().expect("Path is valid Unicode"), fake, "Decoded path shouldn't be different");

        let encoded = encode_uri_path(&path);
        assert_eq!(encoded, fake, "URL encoded alphanumeric String shouldn't change");
    }

    #[test]
    fn uri_enc_dec_roundtrip_invalid_unicode() {
        let base = OsStr::new(&format!("/tmp/{}/", get_unique_name())).to_os_string();

        // Add invalid UTF-8 byte
        let mut bytes = base.into_encoded_bytes();
        bytes.push(168);
        let fake = OsString::from_vec(bytes);
        assert!(fake.to_str().is_none(), "Invalid Unicode cannot be a Rust String");

        let path = decode_uri_path(&fake);
        assert_eq!(path.as_os_str().as_encoded_bytes(), fake.as_encoded_bytes());

        // Shouldn't panic
        encode_uri_path(&path);
    }

    #[test]
    fn test_canonicalize_path_or_parents() {
        enum CanonicalizeFixture<'a> {
            Dir(&'a str),
            File(&'a str),
            Symlink { path: &'a str, target: &'a str },
            Chmod { path: &'a str, mode: u32 },
        }

        enum CanonicalizeExpectation<'a> {
            Canonical(&'a str),
            PermissionDenied,
        }

        struct CanonicalizeCase<'a> {
            name: &'a str,
            setup: &'a [CanonicalizeFixture<'a>],
            input: &'a str,
            expected: CanonicalizeExpectation<'a>,
            teardown: &'a [CanonicalizeFixture<'a>],
        }

        fn resolve(case_root: &Path, relative_path: &str) -> PathBuf {
            case_root.join(relative_path)
        }

        fn apply_fixtures(case_root: &Path, fixtures: &[CanonicalizeFixture<'_>]) {
            for fixture in fixtures {
                match fixture {
                    CanonicalizeFixture::Dir(path) => {
                        std::fs::create_dir_all(resolve(case_root, path)).unwrap();
                    }
                    CanonicalizeFixture::File(path) => {
                        let path = resolve(case_root, path);
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent).unwrap();
                        }
                        File::create_new(path).unwrap();
                    }
                    CanonicalizeFixture::Symlink { path, target } => {
                        let path = resolve(case_root, path);
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent).unwrap();
                        }
                        unix::fs::symlink(resolve(case_root, target), path).unwrap();
                    }
                    CanonicalizeFixture::Chmod { path, mode } => {
                        std::fs::set_permissions(resolve(case_root, path), std::fs::Permissions::from_mode(*mode))
                            .unwrap();
                    }
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cases = [
            CanonicalizeCase {
                name: "path exists",
                setup: &[
                    CanonicalizeFixture::Dir("existing-parent"),
                    CanonicalizeFixture::File("existing-parent/existing-file"),
                ],
                input: "existing-parent/existing-file",
                expected: CanonicalizeExpectation::Canonical("existing-parent/existing-file"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "path doesn't exist",
                setup: &[CanonicalizeFixture::Dir("existing-parent")],
                input: "existing-parent/missing-file",
                expected: CanonicalizeExpectation::Canonical("existing-parent/missing-file"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "path is symlink",
                setup: &[
                    CanonicalizeFixture::Dir("existing-parent"),
                    CanonicalizeFixture::File("existing-parent/path-symlink-target"),
                    CanonicalizeFixture::Symlink {
                        path: "existing-parent/path-symlink",
                        target: "existing-parent/path-symlink-target",
                    },
                ],
                input: "existing-parent/path-symlink",
                expected: CanonicalizeExpectation::Canonical("existing-parent/path-symlink-target"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "no perms to path",
                setup: &[
                    CanonicalizeFixture::Dir("no-perms-path-parent/locked-path"),
                    CanonicalizeFixture::Chmod { path: "no-perms-path-parent/locked-path", mode: 0o000 },
                ],
                input: "no-perms-path-parent/locked-path/child",
                expected: CanonicalizeExpectation::PermissionDenied,
                teardown: &[CanonicalizeFixture::Chmod { path: "no-perms-path-parent/locked-path", mode: 0o700 }],
            },
            CanonicalizeCase {
                name: "parent doesn't exist",
                setup: &[CanonicalizeFixture::Dir("parent-missing-grandparent")],
                input: "parent-missing-grandparent/missing-parent/leaf",
                expected: CanonicalizeExpectation::Canonical("parent-missing-grandparent/missing-parent/leaf"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "parent is symlink",
                setup: &[
                    CanonicalizeFixture::Dir("parent-symlink-target"),
                    CanonicalizeFixture::File("parent-symlink-target/leaf"),
                    CanonicalizeFixture::Dir("parent-symlink-grandparent"),
                    CanonicalizeFixture::Symlink {
                        path: "parent-symlink-grandparent/parent-link",
                        target: "parent-symlink-target",
                    },
                ],
                input: "parent-symlink-grandparent/parent-link/leaf",
                expected: CanonicalizeExpectation::Canonical("parent-symlink-target/leaf"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "parent is symlink but path doesn't exist",
                setup: &[
                    CanonicalizeFixture::Dir("parent-symlink-target"),
                    CanonicalizeFixture::Dir("parent-symlink-grandparent"),
                    CanonicalizeFixture::Symlink {
                        path: "parent-symlink-grandparent/parent-link",
                        target: "parent-symlink-target",
                    },
                ],
                input: "parent-symlink-grandparent/parent-link/missing-leaf",
                expected: CanonicalizeExpectation::Canonical("parent-symlink-target/missing-leaf"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "no perms to parent",
                setup: &[
                    CanonicalizeFixture::Dir("no-perms-parent-grandparent/locked-parent"),
                    CanonicalizeFixture::Chmod { path: "no-perms-parent-grandparent/locked-parent", mode: 0o000 },
                ],
                input: "no-perms-parent-grandparent/locked-parent/leaf/child",
                expected: CanonicalizeExpectation::PermissionDenied,
                teardown: &[CanonicalizeFixture::Chmod {
                    path: "no-perms-parent-grandparent/locked-parent",
                    mode: 0o700,
                }],
            },
            CanonicalizeCase {
                name: "grandparent doesn't exist",
                setup: &[],
                input: "missing-grandparent/parent/leaf",
                expected: CanonicalizeExpectation::Canonical("missing-grandparent/parent/leaf"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "grandparent is symlink",
                setup: &[
                    CanonicalizeFixture::Dir("grandparent-symlink-target/parent"),
                    CanonicalizeFixture::File("grandparent-symlink-target/parent/leaf"),
                    CanonicalizeFixture::Symlink { path: "grandparent-link", target: "grandparent-symlink-target" },
                ],
                input: "grandparent-link/parent/leaf",
                expected: CanonicalizeExpectation::Canonical("grandparent-symlink-target/parent/leaf"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "grandparent is symlink but path doesn't exist",
                setup: &[
                    CanonicalizeFixture::Dir("grandparent-symlink-target/parent"),
                    CanonicalizeFixture::Symlink { path: "grandparent-link", target: "grandparent-symlink-target" },
                ],
                input: "grandparent-link/parent/missing-leaf",
                expected: CanonicalizeExpectation::Canonical("grandparent-symlink-target/parent/missing-leaf"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "grandparent is symlink but parent doesn't exist",
                setup: &[
                    CanonicalizeFixture::Dir("grandparent-symlink-target"),
                    CanonicalizeFixture::Symlink { path: "grandparent-link", target: "grandparent-symlink-target" },
                ],
                input: "grandparent-link/missing-parent/leaf",
                expected: CanonicalizeExpectation::Canonical("grandparent-symlink-target/missing-parent/leaf"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "grandparent is symlink + parent is symlink",
                setup: &[
                    CanonicalizeFixture::Dir("grandparent-symlink-target"),
                    CanonicalizeFixture::Dir("gp-parent-symlink-target"),
                    CanonicalizeFixture::File("gp-parent-symlink-target/leaf"),
                    CanonicalizeFixture::Symlink {
                        path: "grandparent-symlink-target/parent-link",
                        target: "gp-parent-symlink-target",
                    },
                    CanonicalizeFixture::Symlink { path: "grandparent-link", target: "grandparent-symlink-target" },
                ],
                input: "grandparent-link/parent-link/leaf",
                expected: CanonicalizeExpectation::Canonical("gp-parent-symlink-target/leaf"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "grandparent is symlink + path is symlink",
                setup: &[
                    CanonicalizeFixture::Dir("grandparent-symlink-target/parent"),
                    CanonicalizeFixture::File("gp-path-symlink-target"),
                    CanonicalizeFixture::Symlink {
                        path: "grandparent-symlink-target/parent/path-link",
                        target: "gp-path-symlink-target",
                    },
                    CanonicalizeFixture::Symlink { path: "grandparent-link", target: "grandparent-symlink-target" },
                ],
                input: "grandparent-link/parent/path-link",
                expected: CanonicalizeExpectation::Canonical("gp-path-symlink-target"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "grandparent is symlink + parent is symlink + path is symlink",
                setup: &[
                    CanonicalizeFixture::Dir("grandparent-symlink-target"),
                    CanonicalizeFixture::Dir("gp-parent-path-parent-target"),
                    CanonicalizeFixture::Dir("gp-parent-path-symlink-target-parent"),
                    CanonicalizeFixture::File("gp-parent-path-symlink-target-parent/leaf-target"),
                    CanonicalizeFixture::Symlink {
                        path: "gp-parent-path-parent-target/path-link",
                        target: "gp-parent-path-symlink-target-parent/leaf-target",
                    },
                    CanonicalizeFixture::Symlink {
                        path: "grandparent-symlink-target/path-parent-link",
                        target: "gp-parent-path-parent-target",
                    },
                    CanonicalizeFixture::Symlink { path: "grandparent-link", target: "grandparent-symlink-target" },
                ],
                input: "grandparent-link/path-parent-link/path-link",
                expected: CanonicalizeExpectation::Canonical("gp-parent-path-symlink-target-parent/leaf-target"),
                teardown: &[],
            },
            CanonicalizeCase {
                name: "no perms to grandparent",
                setup: &[
                    CanonicalizeFixture::Dir("locked-grandparent/parent"),
                    CanonicalizeFixture::Chmod { path: "locked-grandparent", mode: 0o000 },
                ],
                input: "locked-grandparent/parent/leaf/child",
                expected: CanonicalizeExpectation::PermissionDenied,
                teardown: &[CanonicalizeFixture::Chmod { path: "locked-grandparent", mode: 0o700 }],
            },
        ];

        for (index, case) in cases.iter().enumerate() {
            let case_root = root.join(format!("case-{index}"));
            std::fs::create_dir(&case_root).unwrap();
            apply_fixtures(&case_root, case.setup);

            let input = resolve(&case_root, case.input);
            match case.expected {
                CanonicalizeExpectation::Canonical(expected) => {
                    let expected = resolve(&case_root, expected);
                    let canonical = canonicalize_path_or_parents(&input).unwrap_or_else(|err| {
                        panic!("case `{}` unexpectedly failed for {:?}: {:?}", case.name, input, err);
                    });
                    assert_eq!(canonical, expected, "case `{}` produced an unexpected canonical path", case.name);
                }
                CanonicalizeExpectation::PermissionDenied => match canonicalize_path_or_parents(&input).unwrap_err() {
                    Error::FileSystem { path, source } => {
                        assert_eq!(path, input, "case `{}` returned an unexpected failing path", case.name);
                        assert_eq!(
                            source.kind(),
                            ErrorKind::PermissionDenied,
                            "case `{}` returned an unexpected error",
                            case.name
                        );
                    }
                    err => panic!("case `{}` returned an unexpected error for {:?}: {:?}", case.name, input, err),
                },
            }

            apply_fixtures(&case_root, case.teardown);
        }
    }

    //////////////////////////////////////////////////////////////////////////////////////
    /// System
    //////////////////////////////////////////////////////////////////////////////////////

    #[derive(Debug)]
    pub enum SystemTrashError {
        NoTrashProgram,
        Other(Error),
    }
    impl fmt::Display for SystemTrashError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "SystemTrashError during a `trash` operation: {:?}", self)
        }
    }
    impl std::error::Error for SystemTrashError {}

    fn is_program_in_path(program: &str) -> bool {
        if let Some(path_vars) = std::env::var_os("PATH") {
            for path in std::env::split_paths(&path_vars) {
                let full_path = path.join(program);
                if full_path.is_file() {
                    return true;
                }
            }
        }
        false
    }

    /// This is based on the electron library's implementation.
    /// See: https://github.com/electron/electron/blob/34c4c8d5088fa183f56baea28809de6f2a427e02/shell/common/platform_util_linux.cc#L96
    pub fn delete_all_canonicalized_using_system_program(full_paths: Vec<PathBuf>) -> Result<(), SystemTrashError> {
        static DEFAULT_TRASH: &str = "gio";
        let trash = {
            // Determine desktop environment and set accordingly.
            let desktop_env = get_desktop_environment();
            if desktop_env == DesktopEnvironment::Kde4 || desktop_env == DesktopEnvironment::Kde5 {
                "kioclient5"
            } else if desktop_env == DesktopEnvironment::Kde3 {
                "kioclient"
            } else {
                DEFAULT_TRASH
            }
        };

        let mut argv = Vec::<OsString>::with_capacity(full_paths.len() + 2);

        if trash == "kioclient5" || trash == "kioclient" {
            //argv.push(trash.into());
            argv.push("move".into());
            for full_path in &full_paths {
                argv.push(full_path.into());
            }
            argv.push("trash:/".into());
        } else {
            //argv.push_back(ELECTRON_DEFAULT_TRASH);
            argv.push("trash".into());
            for full_path in &full_paths {
                argv.push(full_path.into());
            }
        }
        if !is_program_in_path(trash) {
            return Err(SystemTrashError::NoTrashProgram);
        }
        // Execute command
        let mut command = Command::new(trash);
        command.args(argv);
        let result = command.output().map_err(|e| {
            SystemTrashError::Other(Error::Unknown {
                description: format!("Tried executing: {:?} - Error was: {}", command, e),
            })
        })?;
        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(SystemTrashError::Other(Error::Unknown {
                description: format!("Used '{}', stderr: {}", trash, stderr),
            }));
        }
        Ok(())
    }

    pub fn delete_all_using_system_program<I, T>(paths: I) -> Result<(), SystemTrashError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<Path>,
    {
        let full_paths = canonicalize_paths(paths).map_err(SystemTrashError::Other)?;
        delete_all_canonicalized_using_system_program(full_paths)
    }

    #[derive(PartialEq)]
    enum DesktopEnvironment {
        Other,
        Cinnamon,
        Gnome,
        // KDE3, KDE4 and KDE5 are sufficiently different that we count
        // them as different desktop environments here.
        Kde3,
        Kde4,
        Kde5,
        Pantheon,
        Unity,
        Xfce,
    }

    fn env_has_var(name: &str) -> bool {
        env::var_os(name).is_some()
    }

    /// See: https://chromium.googlesource.com/chromium/src/+/dd407d416fa941c04e33d81f2b1d8cab8196b633/base/nix/xdg_util.cc#57
    fn get_desktop_environment() -> DesktopEnvironment {
        static KDE_SESSION_ENV_VAR: &str = "KDE_SESSION_VERSION";
        // XDG_CURRENT_DESKTOP is the newest standard circa 2012.
        if let Ok(xdg_current_desktop) = env::var("XDG_CURRENT_DESKTOP") {
            // It could have multiple values separated by colon in priority order.
            for value in xdg_current_desktop.split(':') {
                let value = value.trim();
                if value.is_empty() {
                    continue;
                }
                match value {
                    "Unity" => {
                        // gnome-fallback sessions set XDG_CURRENT_DESKTOP to Unity
                        // DESKTOP_SESSION can be gnome-fallback or gnome-fallback-compiz
                        if let Ok(desktop_session) = env::var("DESKTOP_SESSION") {
                            if desktop_session.contains("gnome-fallback") {
                                return DesktopEnvironment::Gnome;
                            }
                        }
                        return DesktopEnvironment::Unity;
                    }
                    "GNOME" => {
                        return DesktopEnvironment::Gnome;
                    }
                    "X-Cinnamon" => {
                        return DesktopEnvironment::Cinnamon;
                    }
                    "KDE" => {
                        if let Ok(kde_session) = env::var(KDE_SESSION_ENV_VAR) {
                            if kde_session == "5" {
                                return DesktopEnvironment::Kde5;
                            }
                        }
                        return DesktopEnvironment::Kde4;
                    }
                    "Pantheon" => {
                        return DesktopEnvironment::Pantheon;
                    }
                    "XFCE" => {
                        return DesktopEnvironment::Xfce;
                    }
                    _ => {}
                }
            }
        }

        // DESKTOP_SESSION was what everyone  used in 2010.
        if let Ok(desktop_session) = env::var("DESKTOP_SESSION") {
            match desktop_session.as_str() {
                "gnome" | "mate" => {
                    return DesktopEnvironment::Gnome;
                }
                "kde4" | "kde-plasma" => {
                    return DesktopEnvironment::Kde4;
                }
                "kde" => {
                    // This may mean KDE4 on newer systems, so we have to check.
                    if env_has_var(KDE_SESSION_ENV_VAR) {
                        return DesktopEnvironment::Kde4;
                    }
                    return DesktopEnvironment::Kde3;
                }
                "xubuntu" => {
                    return DesktopEnvironment::Xfce;
                }
                _ => {}
            }
            if desktop_session.contains("xfce") {
                return DesktopEnvironment::Xfce;
            }
        }

        // Fall back on some older environment variables.
        // Useful particularly in the DESKTOP_SESSION=default case.
        if env_has_var("GNOME_DESKTOP_SESSION_ID") {
            return DesktopEnvironment::Gnome;
        } else if env_has_var("KDE_FULL_SESSION") {
            if env_has_var(KDE_SESSION_ENV_VAR) {
                return DesktopEnvironment::Kde4;
            }
            return DesktopEnvironment::Kde3;
        }

        DesktopEnvironment::Other
    }
}

fn fs_error(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::FileSystem { path: path.into(), source }
}
