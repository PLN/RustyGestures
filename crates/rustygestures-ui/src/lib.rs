//! Settings UI placeholder.
//!
//! The egui/eframe window is a later milestone. This crate stays free of
//! dependencies so the M0 engine does not link a UI toolkit.

/// The settings UI is not part of the M0 spike.
#[must_use]
pub fn is_implemented() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_is_not_in_the_spike() {
        assert!(!is_implemented());
    }
}
