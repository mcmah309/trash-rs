
[![Crates.io](https://img.shields.io/crates/v/trash?color=mediumvioletred)](https://crates.io/crates/trash)
[![Docs.rs](https://docs.rs/trash/badge.svg)](https://docs.rs/trash)
[![CI](https://github.com/Byron/trash-rs/actions/workflows/rust.yml/badge.svg)](https://github.com/Byron/trash-rs/actions/workflows/rust.yml)


## About

The `trash` is a Rust library for moving files and folders to the operating system's Recycle Bin or Trash or Rubbish Bin or what have you :D

The library supports Windows, macOS, XDG Desktop Portal, and all FreeDesktop Trash compliant environments (including GNOME, KDE, XFCE, and more).
See more about the FreeDesktop Trash implementation in the `freedesktop.rs` file.

On FreeDesktop systems, the default placement continues to prefer trash on the
source filesystem. Applications that need all new deletions in the user's home
trash can opt in per context:

```rust,no_run
use trash::{TrashContext, freedesktop::{TrashContextExtFreedesktop, TrashLocation}};

let mut context = TrashContext::new();
context.set_trash_location(TrashLocation::Home);
context.delete("report.txt")?;
# Ok::<(), trash::Error>(())
```

This API requires the `freedesktop` feature and is available on FreeDesktop Unix
targets. It does not change the Windows, macOS, or Portal backends. Listing,
restoration, and purging still discover entries in every supported trash location.
Home-only placement fails if home trash is unusable, without falling back to a
per-filesystem directory.

Trashing and restoration across filesystems copy the complete payload into
private staging before publishing it without overwriting and removing the source.
This requires space on the destination filesystem. File and directory permissions
and timestamps are preserved; symlinks are copied without following their targets.
Ownership, hard-link relationships, ACLs, and extended attributes are not preserved.
Unsupported special files fail without removing the source. Copy failures remove
partial destinations. If source removal fails after a complete copy, its complete
backup is retained and the operation reports an error. An interrupted copy can
leave a private staging directory while retaining its source.

## Usage

```toml
# In Cargo.toml
[dependencies]
trash = "3"
```

```rust
// In main.rs
use std::fs::File;
use trash;

fn main() {
    // Let's create and remove a single file
    File::create_new("remove-me").unwrap();
    trash::delete("remove-me").unwrap();
    assert!(File::open("remove-me").is_err());

    // Now let's remove multiple files at once
    let the_others = ["remove-me-too", "dont-forget-about-me-either"];
    for name in the_others.iter() {
        File::create_new(name).unwrap();
    }
    trash::delete_all(&the_others).unwrap();
    for name in the_others.iter() {
        assert!(File::open(name).is_err());
    }
}
```
