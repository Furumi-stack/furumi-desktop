//! Narrow adapters for operating-system desktop services.

use std::path::{Path, PathBuf};

mod media;
pub use media::{MediaCommand, MediaSession};

/// Opens the platform-native directory picker.
///
/// The dialog is blocking and must be invoked away from the Slint event loop.
#[must_use]
pub fn choose_library_directory(initial: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Choose Furumi library folder");
    if let Some(path) = initial {
        dialog = dialog.set_directory(path);
    }
    dialog.pick_folder()
}
