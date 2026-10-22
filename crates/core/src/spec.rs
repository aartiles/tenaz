//! `.tenaz/spec.md`: one requirement per line, `- R1: ...`, with
//! `(manual: why)` for what cannot be checked automatically.

use std::sync::LazyLock;

use regex::Regex;

use crate::task::{TaskCard, TaskStatus};

/// One requirement line of the spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    pub id: String,
    pub manual: bool,
    pub text: String,
}

static LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:[-*]\s*)?\**(R\d+)\**\s*[:.)-]").expect("static regex"));
static MANUAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bmanual\s*:").expect("static regex"));

/// Reads the requirements in order; a repeated ID keeps the last line.
pub fn parse_requirements(spec: &str) -> Vec<Requirement> {
    let mut reqs: Vec<Requirement> = Vec::new();
    for line in spec.lines() {
        let Some(caps) = LINE.captures(line) else {
            continue;
        };
        let req = Requirement {
            id: caps[1].to_owned(),
            manual: MANUAL.is_match(line),
            text: line.trim().to_owned(),
        };
        match reqs.iter_mut().find(|r| r.id == req.id) {
            Some(existing) => *existing = req,
            None => reqs.push(req),
        }
    }
    reqs
}

/// The spec with only the requirements in `ids`: other requirement lines
/// (and the lines that continue them) are left out, then the headings left
/// with nothing under them. Prose is kept. `None` when the spec has none of
/// `ids`, so the caller shows the whole spec.
pub fn cited(spec: &str, ids: &[String]) -> Option<String> {
    let mut kept: Vec<&str> = Vec::new();
    let mut found = Vec::new();
    let mut dropping = false;
    for line in spec.lines() {
        let trimmed = line.trim_start();
        if let Some(caps) = LINE.captures(line) {
            let id = caps.get(1).map_or("", |m| m.as_str());
            dropping = !ids.iter().any(|x| x == id);
            if !dropping && !found.contains(&id) {
                found.push(id);
            }
        } else if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
        {
            dropping = false;
        }
        if !dropping {
            kept.push(line);
        }
    }
    if found.is_empty() {
        return None;
    }
    let level = |line: &str| {
        let hashes = line.len() - line.trim_start_matches('#').len();
        (hashes > 0 && line[hashes..].starts_with(' ')).then_some(hashes)
    };
    let mut out: Vec<&str> = Vec::new();
    for (i, line) in kept.iter().enumerate() {
        if let Some(own) = level(line) {
            let has_content = kept[i + 1..]
                .iter()
                .take_while(|next| level(next).is_none_or(|l| l > own))
                .any(|next| level(next).is_none() && !next.trim().is_empty());
            if !has_content {
                continue;
            }
        }
        if line.trim().is_empty() && out.last().is_none_or(|last| last.trim().is_empty()) {
            continue;
        }
        out.push(line);
    }
    let shown = found.join(", ");
    Some(format!(
        "{}\n\nOnly the requirements this task's card requires are shown ({shown}). The whole spec is `.tenaz/spec.md`.",
        out.join("\n").trim_end()
    ))
}

/// Which requirements have a task and which do not.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Coverage {
    pub total: usize,
    pub manual: Vec<String>,
    pub missing: Vec<String>,
}

/// A non-manual requirement is covered when a card that is neither dropped
/// nor split requires it (and, with `done_only`, is done).
pub fn coverage<'a>(
    reqs: &[Requirement],
    tasks: impl IntoIterator<Item = &'a TaskCard>,
    done_only: bool,
) -> Coverage {
    let tasks: Vec<&TaskCard> = tasks.into_iter().collect();
    let mut missing = Vec::new();
    for r in reqs.iter().filter(|r| !r.manual) {
        let covered = tasks.iter().any(|t| {
            t.requires.iter().any(|x| x == &r.id)
                && t.status != TaskStatus::Dropped
                && t.status != TaskStatus::Split
                && (!done_only || t.status == TaskStatus::Done)
        });
        if !covered {
            missing.push(r.id.clone());
        }
    }
    Coverage {
        total: reqs.len(),
        manual: reqs
            .iter()
            .filter(|r| r.manual)
            .map(|r| r.id.clone())
            .collect(),
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requirement_lines_in_several_styles() {
        let reqs = parse_requirements(
            "# Spec\n\n- R1: one\n* **R2**: two\nR3. three\n  - R4 - looks good (manual: visual)\nnot a req\n- R2: two again\n",
        );
        let ids: Vec<_> = reqs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["R1", "R2", "R3", "R4"]);
        assert!(reqs[3].manual);
        assert!(!reqs[0].manual);
        assert_eq!(reqs[1].text, "- R2: two again");
    }

    #[test]
    fn cited_keeps_the_cited_requirements_prose_and_their_headings() {
        let spec = "# Spec\n\nOne line per requirement.\n\n## World\n\n- R1: a town\n  that wraps\n- R2: a desert\n\n## Duel\n\nDuels are one on one.\n\n- R3: a duel\n\n### Draw\n\n- R4: a draw\n\n## Audio\n\n- R5: music\n";
        let ids = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let shown = cited(spec, &ids(&["R2", "R4", "R9"])).unwrap();
        assert_eq!(
            shown,
            "# Spec\n\nOne line per requirement.\n\n## World\n\n- R2: a desert\n\n## Duel\n\nDuels are one on one.\n\n### Draw\n\n- R4: a draw\n\nOnly the requirements this task's card requires are shown (R2, R4). The whole spec is `.tenaz/spec.md`."
        );
        let wrapped = cited(spec, &ids(&["R1"])).unwrap();
        assert!(
            wrapped.contains("- R1: a town\n  that wraps\n"),
            "{wrapped}"
        );
        assert!(!wrapped.contains("## Audio"), "{wrapped}");
        assert_eq!(cited(spec, &ids(&["R9"])), None);
        assert_eq!(cited(spec, &[]), None);
    }

    fn card(id: &str, requires: &[&str], status: TaskStatus) -> TaskCard {
        let text = format!(
            "---\nid: {id}\nstatus: {status}\nrequires: [{}]\n---\n",
            requires.join(", ")
        );
        TaskCard::parse(&format!("{id}.md"), &text).unwrap()
    }

    #[test]
    fn coverage_ignores_manual_dropped_and_split_cards() {
        let reqs = parse_requirements("- R1: a\n- R2: b\n- R3: c (manual: eyes)\n- R4: d\n");
        let tasks = vec![
            card("T001", &["R1"], TaskStatus::Done),
            card("T002", &["R2"], TaskStatus::Dropped),
            card("T003", &["R4"], TaskStatus::Todo),
        ];
        let all = coverage(&reqs, &tasks, false);
        assert_eq!(
            all,
            Coverage {
                total: 4,
                manual: vec!["R3".into()],
                missing: vec!["R2".into()]
            }
        );
        let done = coverage(&reqs, &tasks, true);
        assert_eq!(done.missing, vec!["R2", "R4"]);
    }
}
