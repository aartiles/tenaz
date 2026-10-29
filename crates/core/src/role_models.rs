//! Which model plays each role: the `--model-for ROLE=ID` map `init`
//! checks and pins in `run.json`. A role without an entry is played by
//! the run's model (ADR 0010).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::role::Role;

/// A model that plays one role instead of the run's. It is served by the
/// run's engine, at the run's base URL, through the run's provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleModel {
    /// The model as the harness names it (`provider/id`).
    pub model: String,
    pub id: String,
    pub context_window: u64,
    #[serde(default)]
    pub input: Vec<String>,
}

/// A role's model as `init` was given it, before the defaults fill in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RolePick {
    pub id: String,
    /// `--context-window-for`; without it, the run's window.
    pub context_window: Option<u64>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RoleModelError {
    #[error("{flag} expects ROLE={value}, got {got}")]
    Syntax {
        flag: &'static str,
        value: &'static str,
        got: String,
    },
    #[error("{flag}: unknown role {role}; roles: {roles}", roles = role_names())]
    UnknownRole { flag: &'static str, role: String },
    #[error("{flag} {role} is given twice")]
    Twice { flag: &'static str, role: Role },
    #[error("{MODEL_FLAG} {role}={id} names the run's --model; leave it out")]
    SameAsDefault { role: Role, id: String },
    #[error("{WINDOW_FLAG} {0} needs a {MODEL_FLAG} {0}")]
    WindowWithoutModel(Role),
    #[error("{WINDOW_FLAG} {role} is not a number of tokens: {got}")]
    BadWindow { role: Role, got: String },
}

const MODEL_FLAG: &str = "--model-for";
const WINDOW_FLAG: &str = "--context-window-for";

fn role_names() -> String {
    Role::ALL.map(Role::as_str).join(", ")
}

/// The picks from `--model-for` and `--context-window-for` pairs. A role
/// given twice, a model the same as the run's `default_id`, or a window
/// for a role with no model of its own is an error.
pub fn picks(
    models: &[String],
    windows: &[String],
    default_id: &str,
) -> Result<BTreeMap<Role, RolePick>, RoleModelError> {
    let mut picks = BTreeMap::new();
    for (role, id) in pairs(MODEL_FLAG, "ID", models)? {
        add_model(&mut picks, role, id, default_id)?;
    }
    for (role, raw) in pairs(WINDOW_FLAG, "N", windows)? {
        add_window(&mut picks, role, &raw)?;
    }
    Ok(picks)
}

fn add_model(
    picks: &mut BTreeMap<Role, RolePick>,
    role: Role,
    id: String,
    default_id: &str,
) -> Result<(), RoleModelError> {
    if id == default_id {
        return Err(RoleModelError::SameAsDefault { role, id });
    }
    let pick = RolePick {
        id,
        context_window: None,
    };
    match picks.insert(role, pick) {
        Some(_) => Err(RoleModelError::Twice {
            flag: MODEL_FLAG,
            role,
        }),
        None => Ok(()),
    }
}

fn add_window(
    picks: &mut BTreeMap<Role, RolePick>,
    role: Role,
    raw: &str,
) -> Result<(), RoleModelError> {
    let pick = picks
        .get_mut(&role)
        .ok_or(RoleModelError::WindowWithoutModel(role))?;
    if pick.context_window.is_some() {
        return Err(RoleModelError::Twice {
            flag: WINDOW_FLAG,
            role,
        });
    }
    let window =
        raw.parse::<u64>()
            .ok()
            .filter(|w| *w > 0)
            .ok_or_else(|| RoleModelError::BadWindow {
                role,
                got: raw.to_owned(),
            })?;
    pick.context_window = Some(window);
    Ok(())
}

/// `flag`'s `ROLE=VALUE` pairs, the role parsed; `value` names VALUE in
/// the error.
fn pairs(
    flag: &'static str,
    value: &'static str,
    raw: &[String],
) -> Result<Vec<(Role, String)>, RoleModelError> {
    raw.iter()
        .map(|pair| {
            let (role, given) = pair
                .split_once('=')
                .filter(|(r, v)| !r.is_empty() && !v.is_empty())
                .ok_or_else(|| RoleModelError::Syntax {
                    flag,
                    value,
                    got: pair.clone(),
                })?;
            let role = role
                .parse::<Role>()
                .map_err(|_| RoleModelError::UnknownRole {
                    flag,
                    role: role.to_owned(),
                })?;
            Ok((role, given.to_owned()))
        })
        .collect()
}

/// The models to pin: each pick under `provider`, with the run's window
/// where it gave none. A role's model is taken to read images when the
/// run's does: it gets the run's `input`.
pub fn pin(
    picks: &BTreeMap<Role, RolePick>,
    provider: &str,
    context_window: u64,
    input: &[String],
) -> BTreeMap<Role, RoleModel> {
    picks
        .iter()
        .map(|(role, pick)| {
            let model = RoleModel {
                model: format!("{provider}/{}", pick.id),
                id: pick.id.clone(),
                context_window: pick.context_window.unwrap_or(context_window),
                input: input.to_vec(),
            };
            (*role, model)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| (*s).to_owned()).collect()
    }

    fn pick(id: &str, context_window: Option<u64>) -> RolePick {
        RolePick {
            id: id.into(),
            context_window,
        }
    }

    #[test]
    fn picks_name_a_model_per_role_and_an_optional_window() {
        let picks = picks(
            &strings(&["reviewer=B", "final-reviewer=B", "split=C"]),
            &strings(&["reviewer=65536"]),
            "A",
        )
        .unwrap();
        assert_eq!(
            picks.into_iter().collect::<Vec<_>>(),
            vec![
                (Role::Reviewer, pick("B", Some(65536))),
                (Role::Split, pick("C", None)),
                (Role::FinalReviewer, pick("B", None)),
            ]
        );
    }

    #[test]
    fn a_wrong_pair_is_refused_with_what_is_wrong() {
        let cases: [(&[&str], &[&str], &str); 11] = [
            (
                &["reviewer"],
                &[],
                "--model-for expects ROLE=ID, got reviewer",
            ),
            (
                &["reviewer="],
                &[],
                "--model-for expects ROLE=ID, got reviewer=",
            ),
            (&["=B"], &[], "--model-for expects ROLE=ID, got =B"),
            (
                &["critic=B"],
                &[],
                "--model-for: unknown role critic; roles: bootstrap, builder, reviewer, split, replan, final-reviewer",
            ),
            (
                &["reviewer=B", "reviewer=C"],
                &[],
                "--model-for reviewer is given twice",
            ),
            (
                &["builder=A"],
                &[],
                "--model-for builder=A names the run's --model; leave it out",
            ),
            (
                &["reviewer=B"],
                &["reviewer"],
                "--context-window-for expects ROLE=N, got reviewer",
            ),
            (
                &["reviewer=B"],
                &["split=1000"],
                "--context-window-for split needs a --model-for split",
            ),
            (
                &["reviewer=B"],
                &["reviewer=1", "reviewer=2"],
                "--context-window-for reviewer is given twice",
            ),
            (
                &["reviewer=B"],
                &["reviewer=big"],
                "--context-window-for reviewer is not a number of tokens: big",
            ),
            (
                &["reviewer=B"],
                &["reviewer=0"],
                "--context-window-for reviewer is not a number of tokens: 0",
            ),
        ];
        let wrong: Vec<String> = cases
            .iter()
            .filter_map(|(models, windows, expected)| {
                let got = picks(&strings(models), &strings(windows), "A")
                    .map_or_else(|e| e.to_string(), |p| format!("{p:?}"));
                (got != *expected).then(|| format!("{models:?} {windows:?}: {got}"))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn pinned_models_take_the_run_s_provider_window_and_input() {
        let picks = BTreeMap::from([
            (Role::Reviewer, pick("B", None)),
            (Role::Split, pick("C", Some(32768))),
        ]);
        let input = strings(&["text", "image"]);
        let pinned = pin(&picks, "sushi", 131_072, &input);
        let model = |id: &str, context_window| RoleModel {
            model: format!("sushi/{id}"),
            id: id.into(),
            context_window,
            input: input.clone(),
        };
        assert_eq!(
            pinned.into_iter().collect::<Vec<_>>(),
            vec![
                (Role::Reviewer, model("B", 131_072)),
                (Role::Split, model("C", 32768)),
            ]
        );
    }
}
