//! The first user message of a session. Stable content comes first (role
//! prompt, spec, map) so the engine's prefix cache can reuse it across
//! sessions; then the session header, the task card and the extra sections.

use std::fmt::Write as _;

use crate::role::Role;
use crate::util::clip;

/// A titled block of the briefing. Empty bodies are omitted.
pub fn section(title: &str, text: &str) -> String {
    if text.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n## {title}\n\n{}", text.trim())
    }
}

/// `Role: BUILDER`, the fields, and the one-unit-of-work reminder.
pub fn header(role: Role, fields: &[(String, String)]) -> String {
    let mut lines = vec![format!("Role: {}", role.as_str().to_uppercase())];
    lines.extend(fields.iter().map(|(k, v)| format!("{k}: {v}")));
    lines.push("This is a fresh session. Do only this unit of work, then stop.".to_owned());
    lines.join("\n")
}

/// What a briefing is built from.
#[derive(Clone, Debug, Default)]
pub struct Briefing<'a> {
    pub role_prompt: &'a str,
    /// `.tenaz/spec.md`, when it exists.
    pub spec: Option<&'a str>,
    /// `.tenaz/map.md`, when it exists; clipped to `map_clip_bytes`.
    pub map: Option<&'a str>,
    pub map_clip_bytes: usize,
    /// Pack spec, map and card into the prompt. Off, a builder is only told
    /// where its card is and explores on its own.
    pub packed: bool,
    pub fields: Vec<(String, String)>,
    /// `(card path, card text)`.
    pub card: Option<(&'a str, &'a str)>,
    pub extra: Vec<(String, String)>,
}

/// Composes the prompt as the task loop did.
pub fn build(role: Role, b: &Briefing<'_>) -> String {
    let mut text = b.role_prompt.to_owned();
    if role != Role::Bootstrap && b.packed {
        text += &section("Spec (.tenaz/spec.md)", b.spec.unwrap_or(""));
        text += &section(
            "Repository map (.tenaz/map.md)",
            &clip(b.map.unwrap_or(""), b.map_clip_bytes),
        );
    }
    let _ = write!(text, "\n\n## Session\n\n{}", header(role, &b.fields));
    if let Some((path, card)) = b.card {
        if b.packed {
            text += &section(&format!("Task card ({path})"), card);
        } else {
            let _ = write!(
                text,
                "\n\nYour task card is {path}. Read it, then the spec and map as needed."
            );
        }
    }
    for (title, body) in &b.extra {
        text += &section(title, body);
    }
    text
}

/// The note a fresh session gets when it continues work stopped at the context budget.
pub const CONTINUATION: &str = "## Continuation\n\nAn earlier session on this same work was stopped because its context grew past the budget. Its file changes are kept in the working tree. Start with `git status`, `git diff --stat` and the task's notes file to see what it did, then continue from there. Do not reread large files whole.";

/// The note after the engine refused an image the earlier session read.
pub fn image_continuation(refusal: &str) -> String {
    let quoted: String = refusal
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect();
    format!(
        "## Continuation\n\nAn earlier session on this same work was stopped because the engine refused an image it had read: \"{quoted}\". Every image you read stays in the conversation. Its file changes are kept in the working tree. Start with `git status`, `git diff --stat` and the task's notes file to see what it did, then continue from there. Read only PNG or JPEG images; convert any other format with ffmpeg into `.tenaz/private/` first. Read few images: combine several stills into one small contact sheet (for example with ffmpeg's `tile` filter) and read that, and do not read the same image twice."
    )
}

/// Notes from `BOOTSTRAP`, the dependencies and their split children, each
/// clipped, as `### T003` blocks.
pub fn dependency_notes(
    ids: &[String],
    note_for: impl Fn(&str) -> Option<String>,
    clip_bytes: usize,
) -> String {
    ids.iter()
        .filter_map(|id| {
            note_for(id).map(|note| format!("### {id}\n{}", clip(note.trim(), clip_bytes)))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_builder_briefing_has_stable_content_first() {
        let b = Briefing {
            role_prompt: "ROLE: BUILDER",
            spec: Some("- R1: a"),
            map: Some("- src: code"),
            map_clip_bytes: 8000,
            packed: true,
            fields: vec![
                ("Task".into(), "T007".into()),
                ("Attempt".into(), "1 of 3".into()),
            ],
            card: Some((".tenaz/tasks/T007.md", "---\nid: T007\n---\n## Goal")),
            extra: vec![
                ("Latest ./verify --all".into(), "verify ok".into()),
                ("Empty".into(), "  ".into()),
            ],
        };
        let text = build(Role::Builder, &b);
        assert_eq!(
            text,
            "ROLE: BUILDER\n\n## Spec (.tenaz/spec.md)\n\n- R1: a\n\n## Repository map (.tenaz/map.md)\n\n- src: code\n\n## Session\n\nRole: BUILDER\nTask: T007\nAttempt: 1 of 3\nThis is a fresh session. Do only this unit of work, then stop.\n\n## Task card (.tenaz/tasks/T007.md)\n\n---\nid: T007\n---\n## Goal\n\n## Latest ./verify --all\n\nverify ok"
        );
    }

    #[test]
    fn unpacked_builder_is_pointed_at_its_card() {
        let b = Briefing {
            role_prompt: "P",
            spec: Some("S"),
            map: Some("M"),
            map_clip_bytes: 10,
            packed: false,
            card: Some((".tenaz/tasks/T001.md", "C")),
            ..Default::default()
        };
        let text = build(Role::Builder, &b);
        assert!(!text.contains("## Spec"));
        assert!(text.ends_with(
            "Your task card is .tenaz/tasks/T001.md. Read it, then the spec and map as needed."
        ));
    }

    #[test]
    fn bootstrap_never_gets_spec_or_map_and_the_map_is_clipped() {
        let b = Briefing {
            role_prompt: "P",
            spec: Some("S"),
            map: Some("M"),
            map_clip_bytes: 1,
            packed: true,
            ..Default::default()
        };
        assert!(!build(Role::Bootstrap, &b).contains("## Spec"));
        let long = "abcdef";
        let b = Briefing {
            role_prompt: "P",
            map: Some(long),
            map_clip_bytes: 3,
            packed: true,
            ..Default::default()
        };
        assert!(build(Role::Reviewer, &b).contains("abc\n[... clipped 3 bytes]"));
        assert!(build(Role::FinalReviewer, &b).contains("Role: FINAL-REVIEWER"));
    }

    #[test]
    fn dependency_notes_skip_missing_and_clip() {
        let notes = dependency_notes(
            &["BOOTSTRAP".into(), "T001".into(), "T002".into()],
            |id| match id {
                "BOOTSTRAP" => Some(" stack: shell \n".into()),
                "T002" => Some("x".repeat(20)),
                _ => None,
            },
            12,
        );
        assert_eq!(
            notes,
            "### BOOTSTRAP\nstack: shell\n\n### T002\nxxxxxxxxxxxx\n[... clipped 8 bytes]"
        );
    }

    #[test]
    fn image_continuation_quotes_a_flattened_refusal() {
        let note = image_continuation("too   many\nimages");
        assert!(note.contains("\"too many images\""));
        assert!(CONTINUATION.starts_with("## Continuation"));
    }
}
