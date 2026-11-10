//! The user's own MCP servers, given with `--mcp name=url` (ADR 0032):
//! HTTP servers the agent is offered next to Tenaz's media tools.

use std::collections::BTreeMap;

use crate::ledger::HarnessKind;

/// The name of Tenaz's own media server, which a user's server cannot take.
pub const MEDIA_SERVER: &str = "media";

/// Why a `--mcp` value was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpServerError {
    #[error("--mcp expects name=url, got {0}")]
    NotAPair(String),
    #[error("--mcp name {0:?} may only hold letters, digits, '-' and '_', and must not be empty")]
    BadName(String),
    #[error("--mcp name {MEDIA_SERVER:?} is Tenaz's own media server; pick another")]
    Reserved,
    #[error("--mcp {0} is given twice")]
    Twice(String),
    #[error("--mcp {name} needs an http:// or https:// URL with a host and no spaces, got {url:?}")]
    BadUrl { name: String, url: String },
    #[error("{0} is not given MCP servers of your own (ADR 0032)")]
    NotPassed(HarnessKind),
}

/// The servers `specs` name, by name, each a `name=url` pair whose URL
/// speaks MCP over HTTP.
pub fn parse_mcp_servers(specs: &[String]) -> Result<BTreeMap<String, String>, McpServerError> {
    let mut servers = BTreeMap::new();
    for spec in specs {
        let (name, url) = parse_spec(spec)?;
        if servers.insert(name.to_owned(), url.to_owned()).is_some() {
            return Err(McpServerError::Twice(name.to_owned()));
        }
    }
    Ok(servers)
}

/// One `name=url` pair, checked.
fn parse_spec(spec: &str) -> Result<(&str, &str), McpServerError> {
    let (name, url) = spec
        .split_once('=')
        .ok_or_else(|| McpServerError::NotAPair(spec.to_owned()))?;
    check_pair(name, url)?;
    Ok((name, url))
}

fn check_pair(name: &str, url: &str) -> Result<(), McpServerError> {
    if !valid_name(name) {
        return Err(McpServerError::BadName(name.to_owned()));
    }
    if name == MEDIA_SERVER {
        return Err(McpServerError::Reserved);
    }
    if !valid_url(url) {
        return Err(McpServerError::BadUrl {
            name: name.to_owned(),
            url: url.to_owned(),
        });
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// An `http://` or `https://` URL whose host is not empty, without spaces.
fn valid_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = host_port.split(':').next().unwrap_or_default();
    !host.is_empty() && !url.contains(char::is_whitespace)
}

/// `servers` checked, whether given with `--mcp` or edited into
/// `run.json` by hand, and refused for a harness that would not see them.
pub fn check_servers(
    servers: &BTreeMap<String, String>,
    harness: HarnessKind,
) -> Result<(), McpServerError> {
    for (name, url) in servers {
        check_pair(name, url)?;
    }
    if servers.is_empty() || harness.takes_mcp_servers() {
        Ok(())
    } else {
        Err(McpServerError::NotPassed(harness))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(specs: &[&str]) -> Result<BTreeMap<String, String>, McpServerError> {
        let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
        parse_mcp_servers(&specs)
    }

    fn bad_url(name: &str, url: &str) -> Result<BTreeMap<String, String>, McpServerError> {
        Err(McpServerError::BadUrl {
            name: name.to_owned(),
            url: url.to_owned(),
        })
    }

    #[test]
    fn name_url_pairs_become_servers_by_name() {
        let servers = parse(&[
            "blender=http://127.0.0.1:9191/",
            "docs=https://d.example/mcp",
        ])
        .unwrap();
        assert_eq!(servers["blender"], "http://127.0.0.1:9191/");
        assert_eq!(servers["docs"], "https://d.example/mcp");
        assert!(parse(&[]).unwrap().is_empty());
        assert!(parse(&["blender-lab_2=http://h/"]).is_ok());
    }

    #[test]
    fn a_url_keeps_its_own_equals_signs() {
        let servers = parse(&["s=http://h/mcp?key=v"]).unwrap();
        assert_eq!(servers["s"], "http://h/mcp?key=v");
    }

    #[test]
    fn a_malformed_name_is_refused_with_the_reason() {
        assert_eq!(
            parse(&["http://h"]),
            Err(McpServerError::NotAPair("http://h".to_owned()))
        );
        assert_eq!(
            parse(&["=http://h"]),
            Err(McpServerError::BadName(String::new()))
        );
        assert_eq!(
            parse(&["my server=http://h"]),
            Err(McpServerError::BadName("my server".to_owned()))
        );
        assert_eq!(parse(&["media=http://h"]), Err(McpServerError::Reserved));
        assert_eq!(
            parse(&["b=http://h", "b=http://i"]),
            Err(McpServerError::Twice("b".to_owned()))
        );
    }

    #[test]
    fn a_url_that_is_not_http_with_a_host_is_refused() {
        for url in [
            "uvx blender-mcp",
            "",
            "http://",
            "https:///mcp",
            "http://h /mcp",
            "ftp://h",
            "http://:9191/",
            "http://u@:1/",
        ] {
            assert_eq!(parse(&[&format!("b={url}")]), bad_url("b", url), "{url}");
        }
    }

    #[test]
    fn only_agents_that_are_given_the_servers_accept_them() {
        let servers = parse(&["b=http://h/"]).unwrap();
        for harness in [
            HarnessKind::Fx,
            HarnessKind::OpenCode,
            HarnessKind::Qwen,
            HarnessKind::Acp,
            HarnessKind::Fake,
        ] {
            assert_eq!(check_servers(&servers, harness), Ok(()), "{harness}");
        }
        for harness in [HarnessKind::Pi, HarnessKind::Dsh] {
            assert_eq!(
                check_servers(&servers, harness),
                Err(McpServerError::NotPassed(harness))
            );
            assert_eq!(check_servers(&BTreeMap::new(), harness), Ok(()));
        }
    }

    /// A server edited into `run.json` by hand is held to the same rules
    /// as one given with `--mcp`.
    #[test]
    fn pinned_servers_are_checked_again() {
        let pinned = |name: &str, url: &str| BTreeMap::from([(name.to_owned(), url.to_owned())]);
        assert_eq!(
            check_servers(&pinned("media", "http://h/"), HarnessKind::Fx),
            Err(McpServerError::Reserved)
        );
        assert_eq!(
            check_servers(&pinned("b", "uvx x"), HarnessKind::Fx),
            Err(McpServerError::BadUrl {
                name: "b".to_owned(),
                url: "uvx x".to_owned()
            })
        );
    }
}
