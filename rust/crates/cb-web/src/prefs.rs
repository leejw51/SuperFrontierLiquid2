//! Theme and density, remembered per user.
//!
//! Both are attributes on `<html>` and nothing else: `tokens.css` redefines the
//! whole palette under `[data-theme="light"]` and the whole row-height scale
//! under `[data-density="compact"]`, so switching either is one attribute write
//! and no component re-renders. That is the point of putting the values in
//! custom properties rather than in the components.
//!
//! They are applied before the application mounts, so the first paint is already
//! in the user's chosen theme rather than flashing through the default.

use gloo_storage::{LocalStorage, Storage};

use crate::config::{DENSITY_KEY, THEME_KEY};

/// The stored theme, defaulting to the dark one the design system calls primary.
#[must_use]
pub fn theme() -> String {
    LocalStorage::get::<String>(THEME_KEY).unwrap_or_else(|_| "dark".to_string())
}

/// The stored density.
///
/// Comfortable is the default because a stranger arrives before a professional
/// does; compact is one switch away and remembered.
#[must_use]
pub fn density() -> String {
    LocalStorage::get::<String>(DENSITY_KEY).unwrap_or_else(|_| "comfortable".to_string())
}

/// Writes an attribute onto the document element.
fn set_root_attribute(name: &str, value: &str) {
    if let Some(root) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.document_element())
    {
        let _ = root.set_attribute(name, value);
    }
}

/// Chooses the theme and remembers it.
pub fn set_theme(value: &str) {
    let _ = LocalStorage::set(THEME_KEY, value);
    set_root_attribute("data-theme", value);
}

/// Chooses the density and remembers it.
pub fn set_density(value: &str) {
    let _ = LocalStorage::set(DENSITY_KEY, value);
    set_root_attribute("data-density", value);
}

/// Applies the stored preferences to `<html>`, before the first paint.
pub fn apply_stored() {
    set_root_attribute("data-theme", &theme());
    set_root_attribute("data-density", &density());
}
