//! `init`: a new project from a brief, with the templates copied in and the
//! parameters pinned; also the read-only skill copies and their hashes.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::ledger::{Launch, Ledger, ModelConfig, SkillPin};
use crate::params::RunParams;
use crate::ports::{Clock, Vcs};
use crate::project::Project;
use crate::role::Role;
use crate::util::sha256_hex;

use super::RunError;

/// The shared rules and role prompts copied into every project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Templates {
    pub agents_md: String,
    pub prompts: BTreeMap<Role, String>,
}

impl Templates {
    /// Reads `AGENTS.md` and `prompts/<role>.md` from a directory.
    pub fn load(dir: &Path) -> io::Result<Self> {
        let agents_md = fs::read_to_string(dir.join("AGENTS.md"))?;
        let mut prompts = BTreeMap::new();
        for role in Role::ALL {
            prompts.insert(
                role,
                fs::read_to_string(dir.join("prompts").join(format!("{}.md", role.as_str())))?,
            );
        }
        Ok(Self { agents_md, prompts })
    }

    /// The hash the ledger pins: every prompt in role order, then `AGENTS.md`.
    pub fn sha256(&self) -> String {
        let mut text = String::new();
        for role in Role::ALL {
            text.push_str(self.prompts.get(&role).map_or("", String::as_str));
        }
        text.push_str(&self.agents_md);
        sha256_hex(text)
    }

    fn write_into(&self, project: &Project) -> io::Result<()> {
        project.write("AGENTS.md", &self.agents_md)?;
        for (role, text) in &self.prompts {
            project.write(&role.prompt_path(), text)?;
        }
        Ok(())
    }
}

impl PartialOrd for Role {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Role {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let index = |r: &Role| Role::ALL.iter().position(|x| x == r).unwrap_or(0);
        index(self).cmp(&index(other))
    }
}

/// What `init` needs.
#[derive(Clone, Debug)]
pub struct InitOptions {
    /// Copied to `BRIEF.md`; without it the project must already have one.
    pub brief: Option<PathBuf>,
    pub config: ModelConfig,
    /// How `run` starts the engine, harness and media tools.
    pub launch: Launch,
    pub params: RunParams,
    /// Skill directories (each with a `SKILL.md`) to pin read-only.
    pub skills: Vec<PathBuf>,
    pub runner_version: String,
}

/// Creates the project layout, pins everything in `run.json` and makes the
/// first commit.
pub fn init(
    project: &Project,
    vcs: &dyn Vcs,
    clock: &dyn Clock,
    templates: &Templates,
    options: InitOptions,
) -> Result<Ledger, RunError> {
    if let Some(note) = project.legacy_layout_note() {
        return Err(RunError::Precondition(note));
    }
    if project.exists(".tenaz/run.json") {
        return Err(RunError::Precondition(
            ".tenaz/run.json already exists; this project is already initialized".to_owned(),
        ));
    }
    fs::create_dir_all(project.root())?;
    if let Some(brief) = &options.brief {
        fs::copy(brief, project.path("BRIEF.md"))?;
    }
    let brief = project.read("BRIEF.md").map_err(|_| {
        RunError::Precondition("Pass --brief FILE or put BRIEF.md in the project".to_owned())
    })?;
    if let Some(existing) = project.read_if("AGENTS.md")
        && existing != templates.agents_md
    {
        return Err(RunError::Precondition("The project already has a different AGENTS.md. Move it away first; Tenaz owns that file.".to_owned()));
    }
    project.create_layout()?;
    templates.write_into(project)?;
    project.ensure_gitignore()?;
    if !project.exists(".git") {
        vcs.init()?;
    }
    let skills = options
        .skills
        .iter()
        .map(|s| install_skill(project, s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut ledger = Ledger::new(
        clock.now_rfc3339(),
        sha256_hex(&brief),
        templates.sha256(),
        options.runner_version,
        options.config,
        options.params,
        skills,
    );
    ledger.launch = options.launch;
    project.write(".tenaz/journal.jsonl", "")?;
    project.save_ledger(&ledger)?;
    ledger.checkpoint = Some(vcs.commit_all("tenaz: init")?);
    project.save_ledger(&ledger)?;
    Ok(ledger)
}

/// Copies changed templates into a started run and commits only them.
pub(crate) fn refresh_templates(
    project: &Project,
    vcs: &dyn Vcs,
    templates: &Templates,
) -> Result<(), RunError> {
    templates.write_into(project)?;
    vcs.commit_paths(
        &["AGENTS.md", ".tenaz/prompts"],
        "tenaz: refresh shared rules and prompts",
    )?;
    Ok(())
}

const SKILL_SKIP: [&str; 3] = [".git", ".github", "node_modules"];

/// Files under `dir`, relative and sorted, skipping VCS and dependency folders.
pub(crate) fn tree_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if SKILL_SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out)?;
            } else if path.is_file() {
                out.push(path.strip_prefix(base).unwrap_or(&path).to_path_buf());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

/// A hash over the file names and contents of a tree.
pub(crate) fn tree_hash(dir: &Path) -> io::Result<String> {
    let mut lines = Vec::new();
    for file in tree_files(dir)? {
        lines.push(format!(
            "{}\0{}",
            file.to_string_lossy(),
            sha256_hex(fs::read(dir.join(&file))?)
        ));
    }
    Ok(sha256_hex(lines.join("\n")))
}

/// Copies a skill into `.tenaz/skills/<name>/`, read-only, and hashes it.
fn install_skill(project: &Project, source: &Path) -> Result<SkillPin, RunError> {
    if !source.join("SKILL.md").exists() {
        return Err(RunError::Precondition(format!(
            "{} has no SKILL.md",
            source.display()
        )));
    }
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let to = project.path(&format!(".tenaz/skills/{name}"));
    let files = tree_files(source)?;
    for file in &files {
        let target = to.join(file);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source.join(file), &target)?;
        set_readonly(&target)?;
    }
    let mut dirs: Vec<PathBuf> = files
        .iter()
        .filter_map(|f| f.parent().map(|p| to.join(p)))
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.as_os_str().len()));
    dirs.dedup();
    for dir in dirs.into_iter().chain(std::iter::once(to.clone())) {
        set_readonly(&dir)?;
    }
    Ok(SkillPin {
        name,
        source: source.display().to_string(),
        sha256: tree_hash(&to)?,
        files: files.len(),
    })
}

fn set_readonly(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
}
