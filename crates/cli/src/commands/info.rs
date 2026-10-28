//! `tenaz params` and `tenaz guide`: what this binary knows, printed.

use tenaz_core::params::RunParams;

/// `tenaz params`: every parameter with its default, `key=value`.
pub(crate) fn params() -> anyhow::Result<()> {
    let json = serde_json::to_value(RunParams::default())?;
    if let Some(map) = json.as_object() {
        for (key, value) in map {
            println!("{key}={value}");
        }
    }
    Ok(())
}

/// `tenaz guide`: the agent skill this binary was built with.
pub(crate) fn guide() {
    print!("{}", include_str!("../../../../skills/tenaz/SKILL.md"));
}
