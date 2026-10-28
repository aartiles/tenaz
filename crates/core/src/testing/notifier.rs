//! A notifier for tests.

use crate::ports::Notifier;

/// Drops every notification.
#[derive(Clone, Copy, Debug, Default)]
pub struct SilentNotifier;

impl Notifier for SilentNotifier {
    fn notify(&self, _message: &str) {}
}
