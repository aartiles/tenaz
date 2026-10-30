//! Which model plays each role: the `--model-for ROLE=ID` and
//! `--context-window-for ROLE=N` pairs `init` checks and pins in
//! `run.json`. A role without an entry is played by
//! the run's model (ADR 0011).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ports::SessionModel;
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

/// The `input` entry of a model that reads images.
pub const IMAGE_INPUT: &str = "image";

impl RoleModel {
    pub fn supports_images(&self) -> bool {
        self.input.iter().any(|i| i == IMAGE_INPUT)
    }

    pub fn session_model(&self) -> SessionModel {
        SessionModel {
            name: self.model.clone(),
            id: self.id.clone(),
            context_window: self.context_window,
            supports_images: self.supports_images(),
        }
    }
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
    #[error("{} expects ROLE={}, got {got}", flag.name, flag.value)]
    Syntax { flag: &'static Flag, got: String },
    #[error("{}: unknown role {role}; roles: {roles}", flag.name, roles = role_names())]
    UnknownRole { flag: &'static Flag, role: String },
    #[error("{} {role} is given twice", flag.name)]
    Twice { flag: &'static Flag, role: Role },
    #[error("{} {role}={id} names the run's --model; leave it out", MODEL.name)]
    SameAsDefault { role: Role, id: String },
    #[error("{window} {0} needs a {model} {0}", window = WINDOW.name, model = MODEL.name)]
    WindowWithoutModel(Role),
    #[error("{} {role} is not a positive number of tokens: {got}", WINDOW.name)]
    BadWindow { role: Role, got: String },
    #[error(
        "{model} is given to {role} and {other} with different windows; give both the same {}, or neither",
        WINDOW.name
    )]
    WindowDiffers {
        model: String,
        role: Role,
        other: Role,
    },
}

/// A flag of `ROLE=VALUE` pairs, and what its errors call VALUE.
#[derive(Debug, PartialEq, Eq)]
pub struct Flag {
    pub name: &'static str,
    pub value: &'static str,
}

const MODEL: Flag = Flag {
    name: "--model-for",
    value: "ID",
};
const WINDOW: Flag = Flag {
    name: "--context-window-for",
    value: "N",
};

fn role_names() -> String {
    Role::ALL.map(Role::as_str).join(", ")
}

/// The `--model-for` and `--context-window-for` pairs as given.
#[derive(Clone, Copy, Debug)]
pub struct RoleFlags<'a> {
    pub models: &'a [String],
    pub windows: &'a [String],
}

impl RoleFlags<'_> {
    /// The picks these flags make. A pair that is not `ROLE=VALUE`, an
    /// unknown role, a role given twice, a model the same as the run's
    /// `default_id`, a window for a role with no model of its own, a
    /// window that is not a positive number, and one model given different
    /// windows for two roles are errors.
    pub fn picks(self, default_id: &str) -> Result<BTreeMap<Role, RolePick>, RoleModelError> {
        let mut picks = BTreeMap::new();
        for (role, id) in pairs(&MODEL, self.models)? {
            if id == default_id {
                return Err(RoleModelError::SameAsDefault { role, id });
            }
            let pick = RolePick {
                id,
                context_window: None,
            };
            if picks.insert(role, pick).is_some() {
                return Err(RoleModelError::Twice { flag: &MODEL, role });
            }
        }
        for (role, raw) in pairs(&WINDOW, self.windows)? {
            add_window(&mut picks, role, &raw)?;
        }
        one_window_per_model(&picks)?;
        Ok(picks)
    }
}

/// The engine holds a model with one window: roles that name the same
/// model must give it the same one.
fn one_window_per_model(picks: &BTreeMap<Role, RolePick>) -> Result<(), RoleModelError> {
    let mut first: BTreeMap<&str, (Role, Option<u64>)> = BTreeMap::new();
    for (role, pick) in picks {
        let (other, window) = *first
            .entry(pick.id.as_str())
            .or_insert((*role, pick.context_window));
        if window != pick.context_window {
            return Err(RoleModelError::WindowDiffers {
                model: pick.id.clone(),
                role: other,
                other: *role,
            });
        }
    }
    Ok(())
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
            flag: &WINDOW,
            role,
        });
    }
    pick.context_window = Some(parse_window(role, raw)?);
    Ok(())
}

fn parse_window(role: Role, raw: &str) -> Result<u64, RoleModelError> {
    match raw.parse::<u64>() {
        Ok(window) if window > 0 => Ok(window),
        _ => Err(RoleModelError::BadWindow {
            role,
            got: raw.to_owned(),
        }),
    }
}

/// `flag`'s `ROLE=VALUE` pairs, the role parsed.
fn pairs(flag: &'static Flag, raw: &[String]) -> Result<Vec<(Role, String)>, RoleModelError> {
    raw.iter()
        .map(|pair| {
            let (role, given) = pair
                .split_once('=')
                .filter(|(r, v)| !r.is_empty() && !v.is_empty())
                .ok_or_else(|| RoleModelError::Syntax {
                    flag,
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

/// What a role's model takes from the run's.
#[derive(Clone, Copy, Debug)]
pub struct RunDefaults<'a> {
    pub provider: &'a str,
    pub context_window: u64,
    pub input: &'a [String],
}

/// The models to pin: each pick under the run's provider, with the run's
/// window where it gave none. A role's model is taken to read images when
/// the run's does: it gets the run's `input`.
pub fn pin(picks: &BTreeMap<Role, RolePick>, run: RunDefaults<'_>) -> BTreeMap<Role, RoleModel> {
    picks
        .iter()
        .map(|(role, pick)| {
            let model = RoleModel {
                model: format!("{}/{}", run.provider, pick.id),
                id: pick.id.clone(),
                context_window: pick.context_window.unwrap_or(run.context_window),
                input: run.input.to_vec(),
            };
            (*role, model)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(id: &str, context_window: Option<u64>) -> RolePick {
        RolePick {
            id: id.into(),
            context_window,
        }
    }

    #[test]
    fn picks_name_a_model_per_role_and_an_optional_window() {
        let flags = RoleFlags {
            models: &["reviewer=B", "final-reviewer=B", "split=C"].map(String::from),
            windows: &["reviewer=65536", "final-reviewer=65536"].map(String::from),
        };
        let picks = flags.picks("A").unwrap();
        assert_eq!(
            picks.into_iter().collect::<Vec<_>>(),
            vec![
                (Role::Reviewer, pick("B", Some(65536))),
                (Role::Split, pick("C", None)),
                (Role::FinalReviewer, pick("B", Some(65536))),
            ]
        );
    }

    /// `--model-for` and `--context-window-for` pairs, and the error each gives.
    const WRONG: [(&[&str], &[&str], &str); 12] = [
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
            "--context-window-for reviewer is not a positive number of tokens: big",
        ),
        (
            &["reviewer=B"],
            &["reviewer=0"],
            "--context-window-for reviewer is not a positive number of tokens: 0",
        ),
        (
            &["reviewer=B", "final-reviewer=B"],
            &["reviewer=65536"],
            "B is given to reviewer and final-reviewer with different windows; give both the same --context-window-for, or neither",
        ),
    ];

    /// Two roles on one model with no window of their own both take the
    /// run's.
    #[test]
    fn roles_on_one_model_may_both_leave_the_window_out() {
        let flags = RoleFlags {
            models: &["reviewer=B", "final-reviewer=B"].map(String::from),
            windows: &[],
        };
        let picks = flags.picks("A").unwrap();
        assert_eq!(
            picks.into_values().collect::<Vec<_>>(),
            vec![pick("B", None), pick("B", None)]
        );
    }

    #[test]
    fn a_wrong_pair_is_refused_with_what_is_wrong() {
        let wrong: Vec<String> = WRONG
            .iter()
            .filter_map(|(models, windows, expected)| {
                let owned = |raw: &[&str]| raw.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
                let (models, windows) = (owned(models), owned(windows));
                let flags = RoleFlags {
                    models: &models,
                    windows: &windows,
                };
                let got = flags
                    .picks("A")
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
        let input = ["text", "image"].map(String::from).to_vec();
        let pinned = pin(
            &picks,
            RunDefaults {
                provider: "sushi",
                context_window: 131_072,
                input: &input,
            },
        );
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
