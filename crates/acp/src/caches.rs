//! The user's own package and browser caches, for an agent that runs with
//! a `HOME` of its own: `verify` runs with the user's `HOME`, so what the
//! agent installs there must land where `verify` looks for it.

use std::path::Path;

/// `PLAYWRIGHT_BROWSERS_PATH` and `npm_config_cache` for an agent with an
/// isolated `HOME`: the user's own setting when `env` has a non-empty one
/// (npm also reads `NPM_CONFIG_CACHE`), else where Playwright and npm
/// look under the user's `home` (Playwright under `Library/Caches` on
/// macOS, under `XDG_CACHE_HOME` or `.cache` elsewhere). Without the
/// user's own settings (`XDG_CACHE_HOME` included), both are in the
/// sandbox's default write roots.
pub fn user_caches(
    home: &Path,
    env: impl Fn(&str) -> Option<String>,
    macos: bool,
) -> Vec<(String, String)> {
    let set = |name: &str| env(name).filter(|v| !v.is_empty());
    let playwright = set("PLAYWRIGHT_BROWSERS_PATH").unwrap_or_else(|| {
        let caches = if macos {
            home.join("Library/Caches")
        } else {
            set("XDG_CACHE_HOME").map_or_else(|| home.join(".cache"), Into::into)
        };
        caches.join("ms-playwright").display().to_string()
    });
    let npm = set("npm_config_cache")
        .or_else(|| set("NPM_CONFIG_CACHE"))
        .unwrap_or_else(|| home.join(".npm").display().to_string());
    vec![
        ("PLAYWRIGHT_BROWSERS_PATH".to_owned(), playwright),
        ("npm_config_cache".to_owned(), npm),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYWRIGHT: &str = "PLAYWRIGHT_BROWSERS_PATH";
    const NPM: &str = "npm_config_cache";

    /// The value `user_caches` gives `name`.
    fn cache(home: &str, env: impl Fn(&str) -> Option<String>, macos: bool, name: &str) -> String {
        user_caches(Path::new(home), env, macos)
            .into_iter()
            .find_map(|(n, value)| (n == name).then_some(value))
            .unwrap_or_else(|| panic!("no {name}"))
    }

    /// An environment with only `vars` set.
    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect();
        move |name| vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn by_default_the_caches_are_where_playwright_and_npm_put_them_on_macos() {
        let xdg = env(&[("XDG_CACHE_HOME", "/var/cache/me")]);
        assert_eq!(
            cache("/Users/me", &xdg, true, PLAYWRIGHT),
            "/Users/me/Library/Caches/ms-playwright",
            "macOS ignores XDG_CACHE_HOME"
        );
        assert_eq!(cache("/Users/me", env(&[]), true, NPM), "/Users/me/.npm");
    }

    #[test]
    fn elsewhere_playwright_follows_the_xdg_cache() {
        assert_eq!(
            cache("/home/me", env(&[]), false, PLAYWRIGHT),
            "/home/me/.cache/ms-playwright"
        );
        assert_eq!(
            cache(
                "/home/me",
                env(&[("XDG_CACHE_HOME", "/var/cache/me")]),
                false,
                PLAYWRIGHT
            ),
            "/var/cache/me/ms-playwright"
        );
    }

    #[test]
    fn an_empty_setting_is_no_setting() {
        let empty = env(&[(PLAYWRIGHT, ""), (NPM, "")]);
        assert_eq!(
            cache("/Users/me", &empty, true, PLAYWRIGHT),
            "/Users/me/Library/Caches/ms-playwright"
        );
        assert_eq!(cache("/Users/me", &empty, true, NPM), "/Users/me/.npm");
        assert_eq!(
            cache(
                "/home/me",
                env(&[("XDG_CACHE_HOME", "")]),
                false,
                PLAYWRIGHT
            ),
            "/home/me/.cache/ms-playwright",
            "an empty XDG_CACHE_HOME is no setting either"
        );
    }

    #[test]
    fn npm_s_upper_case_setting_counts_after_the_lower_case_one() {
        assert_eq!(
            cache(
                "/Users/me",
                env(&[("NPM_CONFIG_CACHE", "/opt/upper")]),
                true,
                NPM
            ),
            "/opt/upper"
        );
        let both = env(&[("NPM_CONFIG_CACHE", "/opt/upper"), (NPM, "/opt/lower")]);
        assert_eq!(cache("/Users/me", both, true, NPM), "/opt/lower");
    }

    #[test]
    fn the_user_s_own_settings_are_kept() {
        let own = env(&[(PLAYWRIGHT, "/opt/browsers"), (NPM, "/opt/npm")]);
        assert_eq!(cache("/Users/me", &own, true, PLAYWRIGHT), "/opt/browsers");
        assert_eq!(cache("/Users/me", &own, true, NPM), "/opt/npm");
    }
}
