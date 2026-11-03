//! The pure parts of the `generate_3d` tool (ADR 0020): which paths the
//! agent may name, and what counts as a GLB.

use std::path::{Component, Path, PathBuf};

/// Words in a model id that mark a 3D model: the families mlx-serve serves.
/// A heuristic: `--mesh-model` names any other.
const MESH_MODEL_WORDS: [&str; 4] = ["trellis", "idol", "hunyuan3d", "pixal3d"];

/// Whether a model id looks like an image-to-3D model.
pub fn is_mesh_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    MESH_MODEL_WORDS.iter().any(|word| id.contains(word))
}

/// Whether `bytes` start as a binary glTF file does.
pub fn is_glb(bytes: &[u8]) -> bool {
    bytes.starts_with(b"glTF")
}

/// Whether `bytes` start as a PNG or a JPEG does: the only pictures the
/// tool sends, so no other project file leaves through it.
pub fn is_picture(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(b"\xFF\xD8\xFF")
}

/// `path`, relative to `root` or absolute under it, as an absolute path
/// inside `root` (absolute), resolved without touching the disk. Refused
/// when it leaves the project or goes into `.git` or `.tenaz`. Symbolic
/// links are not followed here: the caller checks them on the disk.
pub fn in_project(root: &Path, path: &str) -> Result<PathBuf, String> {
    let given = Path::new(path.trim());
    if given.as_os_str().is_empty() {
        return Err("the path is empty".to_owned());
    }
    let relative = if given.is_absolute() {
        given
            .strip_prefix(root)
            .map_err(|_| format!("{path} is outside the project"))?
    } else {
        given
    };
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                parts
                    .pop()
                    .ok_or_else(|| format!("{path} is outside the project"))?;
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("{path} is outside the project"));
            }
        }
    }
    if parts.is_empty() {
        return Err(format!("{path} is the project itself, not a file"));
    }
    let first = parts[0].to_string_lossy();
    if first.eq_ignore_ascii_case(".git") || first.eq_ignore_ascii_case(".tenaz") {
        return Err(format!(
            "{path} is inside {}, which is not the agent's",
            parts[0].display()
        ));
    }
    Ok(parts
        .iter()
        .fold(root.to_path_buf(), |p, part| p.join(part)))
}

/// Where the GLB the agent asked for goes: `requested` inside the
/// project, ending in `.glb`.
pub fn mesh_output(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let path = in_project(root, requested)?;
    let glb = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("glb"));
    if glb {
        Ok(path)
    } else {
        Err(format!("{requested} must end in .glb"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/work/cabinet";

    fn root() -> &'static Path {
        Path::new(ROOT)
    }

    #[test]
    fn mesh_models_are_told_apart_by_name() {
        for id in [
            "TRELLIS.2-4B-MLX-Serve-8bit",
            "ddalcu/Idol-MLX-Serve-8bit",
            "tencent/Hunyuan3D-2.1",
            "TencentARC/Pixal3D",
        ] {
            assert!(is_mesh_model(id), "{id}");
        }
        for id in [
            "Qwen3.8-27B-MLX-Serve-4bit",
            "flux2-klein",
            "qwen-image-2.1",
        ] {
            assert!(!is_mesh_model(id), "{id}");
        }
    }

    #[test]
    fn a_glb_starts_with_its_magic() {
        assert!(is_glb(b"glTF\x02\0\0\0rest"));
        assert!(!is_glb(b"{\"asset\":{}}"));
        assert!(!is_glb(b""));
    }

    #[test]
    fn only_a_png_or_a_jpeg_is_a_picture() {
        assert!(is_picture(b"\x89PNG\r\n\x1a\nrest"));
        assert!(is_picture(b"\xFF\xD8\xFF\xE0rest"));
        assert!(!is_picture(b"API_KEY=secret"));
        assert!(!is_picture(b"RIFF\0\0\0\0WEBPVP8 "));
    }

    /// Only the top-level `.git` is the repository's; a nested one is a file.
    #[test]
    fn a_git_folder_below_the_top_is_a_project_path() {
        assert_eq!(
            in_project(root(), "vendor/.git/config"),
            Ok(PathBuf::from("/work/cabinet/vendor/.git/config"))
        );
        assert!(in_project(root(), "/work/cabinet").is_err());
    }

    #[test]
    fn paths_inside_the_project_resolve_under_its_root() {
        let expected = PathBuf::from("/work/cabinet/photos/cabinet.png");
        for path in [
            "photos/cabinet.png",
            "./photos/./cabinet.png",
            "photos/old/../cabinet.png",
            "/work/cabinet/photos/cabinet.png",
            " photos/cabinet.png ",
        ] {
            assert_eq!(in_project(root(), path), Ok(expected.clone()), "{path}");
        }
    }

    #[test]
    fn paths_out_of_the_project_or_into_git_are_refused() {
        for path in [
            "../other/cabinet.png",
            "photos/../../cabinet.png",
            "/etc/passwd",
            "/work/cabinet-other/x.png",
            ".git/config",
            "photos/../.git/HEAD",
            ".tenaz/run.json",
            ".GIT/config",
            "",
            ".",
            "photos/..",
        ] {
            assert!(in_project(root(), path).is_err(), "{path}");
        }
    }

    #[test]
    fn the_output_is_a_glb_in_the_project() {
        assert_eq!(
            mesh_output(root(), "assets/cabinet.GLB"),
            Ok(PathBuf::from("/work/cabinet/assets/cabinet.GLB"))
        );
        let not_glb = mesh_output(root(), "assets/cabinet.obj").unwrap_err();
        assert!(not_glb.contains(".glb"), "{not_glb}");
        assert!(mesh_output(root(), "../cabinet.glb").is_err());
        assert!(mesh_output(root(), ".tenaz/run.glb").is_err());
    }
}
