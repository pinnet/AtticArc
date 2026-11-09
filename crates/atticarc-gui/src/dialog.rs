//! Folder dialogs, modal to the AtticARC window.
//!
//! With the window as its parent, Windows disables the window while a dialog
//! is open: only one dialog at a time, and no click can reach the window
//! behind it (such as a second Change… button under the dialog).

use std::cell::Cell;
use std::path::{Path, PathBuf};

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle, WindowHandle,
};

thread_local! {
    /// The window's handles, set at the start of every frame on the UI thread.
    static PARENT: Cell<Option<(RawWindowHandle, RawDisplayHandle)>> = const { Cell::new(None) };
}

/// Records the window to parent dialogs to. Called at the start of each frame.
pub fn set_parent(frame: &eframe::Frame) {
    let handles = match (frame.window_handle(), frame.display_handle()) {
        (Ok(w), Ok(d)) => Some((w.as_raw(), d.as_raw())),
        _ => None,
    };
    PARENT.with(|p| p.set(handles));
}

/// The window, for `rfd`. Only built during a frame, while the window exists.
struct Parent(RawWindowHandle, RawDisplayHandle);

impl HasWindowHandle for Parent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: the handle came from eframe this frame, the window outlives
        // the frame, and the dialog is shown and closed within the frame.
        Ok(unsafe { WindowHandle::borrow_raw(self.0) })
    }
}

impl HasDisplayHandle for Parent {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // SAFETY: as above.
        Ok(unsafe { DisplayHandle::borrow_raw(self.1) })
    }
}

/// Opens a modal folder dialog titled `title`, starting in `start` if given.
pub fn pick_folder(title: &str, start: Option<&Path>) -> Option<PathBuf> {
    let mut d = rfd::FileDialog::new().set_title(title);
    if let Some(start) = start {
        d = d.set_directory(start);
    }
    if let Some((w, disp)) = PARENT.with(Cell::get) {
        d = d.set_parent(&Parent(w, disp));
    }
    d.pick_folder()
}
