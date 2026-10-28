//! Where a session may write without the sandbox: the places the sandbox
//! allows, which the sandbox adapter lists. A write tool's target outside
//! them is noted, not stopped (ADR 0008). Only the write tools' own path
//! arguments are checked: what a shell command writes cannot be told from
//! its text.

use std::path::{Component, Path, PathBuf};

/// The arguments a write tool names its file in: `path` (Pi, fx) or
/// `file_path`.
const PATH_KEYS: [&str; 2] = ["path", "file_path"];

/// The directories a session may write to, and how to resolve the paths it
/// names. Sessions run in the project root: relative paths resolve there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteScope {
    /// The project root, as the session is told it.
    root: PathBuf,
    /// For `~` in a path the session names.
    home: Option<PathBuf>,
    /// Every directory writes may go to, with everything below it.
    allowed: Vec<PathBuf>,
}

impl WriteScope {
    /// The scope of the project at `root` (absolute), which may also write
    /// to `others` (absolute; a directory reached through a symbolic link,
    /// such as `/tmp`, is given both as written and as resolved).
    pub fn new(root: &Path, home: Option<PathBuf>, others: Vec<PathBuf>) -> Self {
        let mut allowed = vec![root.to_path_buf()];
        allowed.extend(others);
        Self {
            root: root.to_path_buf(),
            home,
            allowed,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file a write tool's `args` name, resolved, when it is outside
    /// every allowed directory; `None` when it is inside one or names none.
    pub fn outside(&self, args: &serde_json::Value) -> Option<String> {
        let named = PATH_KEYS
            .iter()
            .find_map(|key| args.get(*key).and_then(serde_json::Value::as_str))?;
        let path = self.resolve(named)?;
        let inside = self.allowed.iter().any(|dir| path.starts_with(dir));
        (!inside).then(|| path.display().to_string())
    }

    /// `named` made absolute against the project root, with `~` expanded
    /// and `.` and `..` taken out as written: the files need not exist yet.
    fn resolve(&self, named: &str) -> Option<PathBuf> {
        let expanded = expand_home(named, self.home.as_deref())?;
        Some(normalize(&self.root.join(expanded)))
    }
}

/// `entry` with a leading `~` replaced by `home`; `None` when it has one
/// and there is no home.
fn expand_home(entry: &str, home: Option<&Path>) -> Option<PathBuf> {
    match entry.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            Some(home?.join(rest.trim_start_matches('/')))
        }
        _ => Some(PathBuf::from(entry)),
    }
}

/// `path` without `.` and `..`, without looking at the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> WriteScope {
        WriteScope::new(
            Path::new("/work/repo/app"),
            Some(PathBuf::from("/home/me")),
            vec![PathBuf::from("/tmp"), PathBuf::from("/home/me/.cache")],
        )
    }

    fn outside(path: &str) -> Option<String> {
        scope().outside(&json!({ "path": path }))
    }

    #[test]
    fn a_relative_path_in_the_project_is_inside() {
        assert_eq!(outside(".tenaz/spec.md"), None);
        assert_eq!(outside("./src/../.tenaz/map.md"), None);
        assert_eq!(outside("/work/repo/app"), None);
    }

    #[test]
    fn a_path_up_from_the_project_is_outside() {
        assert_eq!(
            outside("../.tenaz/spec.md").as_deref(),
            Some("/work/repo/.tenaz/spec.md")
        );
    }

    #[test]
    fn a_path_that_climbs_past_the_top_stops_there() {
        assert_eq!(outside("../../../../../etc/x").as_deref(), Some("/etc/x"));
    }

    #[test]
    fn a_path_in_the_home_directory_is_outside() {
        assert_eq!(
            outside("~/.tenaz/map.md").as_deref(),
            Some("/home/me/.tenaz/map.md")
        );
        assert_eq!(
            outside("/home/me/.tenaz/map.md").as_deref(),
            Some("/home/me/.tenaz/map.md")
        );
    }

    #[test]
    fn the_other_allowed_directories_are_inside() {
        assert_eq!(outside("/tmp/scratch.txt"), None);
        assert_eq!(outside("~/.cache/tool/x"), None);
    }

    #[test]
    fn a_sibling_whose_name_starts_like_the_project_is_outside() {
        assert!(outside("/work/repo/application/x").is_some());
    }

    #[test]
    fn file_path_is_read_when_there_is_no_path() {
        let args = json!({ "file_path": "/etc/hosts" });
        assert_eq!(scope().outside(&args).as_deref(), Some("/etc/hosts"));
        let both = json!({ "path": "src/x", "file_path": "/etc/hosts" });
        assert_eq!(scope().outside(&both), None, "path comes first");
    }

    #[test]
    fn a_call_that_names_no_path_is_not_judged() {
        assert_eq!(scope().outside(&json!({ "content": "x" })), None);
    }

    #[test]
    fn a_tilde_without_a_home_is_not_judged() {
        let homeless = WriteScope::new(Path::new("/p"), None, Vec::new());
        assert_eq!(homeless.outside(&json!({ "path": "~/x" })), None);
    }

    #[test]
    fn a_tilde_with_a_user_name_is_a_name_in_the_project() {
        assert_eq!(outside("~other/x"), None);
    }
}
