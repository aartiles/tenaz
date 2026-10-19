//! Desktop notifications, so whoever shares the machine knows a run holds the
//! engine and most of its memory.

use tenaz_core::ports::Notifier;

/// macOS notifications through `osascript`; silent elsewhere.
#[derive(Clone, Debug)]
pub struct DesktopNotifier {
    subtitle: String,
}

impl DesktopNotifier {
    /// `subtitle` names the run (the project's parent folder, as the task loop did).
    pub fn new(subtitle: impl Into<String>) -> Self {
        Self {
            subtitle: subtitle.into(),
        }
    }
}

fn quote(text: &str) -> String {
    let short: String = text.chars().take(200).collect();
    format!("\"{}\"", short.replace('\\', "\\\\").replace('"', "\\\""))
}

impl Notifier for DesktopNotifier {
    fn notify(&self, message: &str) {
        if !cfg!(target_os = "macos") {
            return;
        }
        let script = format!(
            "display notification {} with title \"Task loop\" subtitle {}",
            quote(message),
            quote(&self.subtitle)
        );
        if let Ok(mut child) = std::process::Command::new("osascript")
            .args(["-e", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Drops every notification.
#[derive(Clone, Copy, Debug, Default)]
pub struct SilentNotifier;

impl Notifier for SilentNotifier {
    fn notify(&self, _message: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes_and_shortens() {
        assert_eq!(
            quote("say \"hi\" \\ there"),
            "\"say \\\"hi\\\" \\\\ there\""
        );
        assert_eq!(quote(&"x".repeat(300)).len(), 202);
    }
}
