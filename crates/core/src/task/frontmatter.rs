//! The task card's frontmatter: a small `key: value` dialect the model
//! writes by hand. Lists are `[a, b]`, maps are `{turns: 40, tokens: 60000}`,
//! numbers and booleans are bare, strings may be quoted. Comments after `#`
//! are dropped. This is the task loop's parser, kept byte for byte.

use thiserror::Error;

/// A parsed frontmatter value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Text(String),
    Number(f64),
    Bool(bool),
    List(Vec<String>),
    Map(Vec<(String, Value)>),
}

impl Value {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// A text value, or a number or boolean rendered as text.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Value::Text(s) => Some(s.clone()),
            Value::Number(n) => Some(format_number(*n)),
            Value::Bool(b) => Some(b.to_string()),
            Value::List(_) | Value::Map(_) => None,
        }
    }

    /// A list of strings. A single text value counts as a one-element list,
    /// as `[].concat(x)` did in the task loop.
    pub fn to_list(&self) -> Vec<String> {
        match self {
            Value::List(items) => items.clone(),
            Value::Text(s) if s.is_empty() => Vec::new(),
            other => other.to_text().into_iter().collect(),
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "only whole numbers below 1e15 are cast, and they fit i64 exactly"
)]
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

fn strip_quotes(s: &str) -> String {
    let s = s
        .strip_prefix('"')
        .or_else(|| s.strip_prefix('\''))
        .unwrap_or(s);
    let s = s
        .strip_suffix('"')
        .or_else(|| s.strip_suffix('\''))
        .unwrap_or(s);
    s.to_owned()
}

/// Parses one value as the task loop did.
pub fn parse_value(raw: &str) -> Value {
    let v = raw.trim();
    if v.len() >= 2 && v.starts_with('[') && v.ends_with(']') {
        return Value::List(
            v[1..v.len() - 1]
                .split(',')
                .map(|s| strip_quotes(s.trim()))
                .filter(|s| !s.is_empty())
                .collect(),
        );
    }
    if v.len() >= 2 && v.starts_with('{') && v.ends_with('}') {
        let entries = v[1..v.len() - 1]
            .split(',')
            .filter_map(|pair| {
                let (k, x) = pair.split_once(':').unwrap_or((pair, ""));
                let k = k.trim();
                (!k.is_empty()).then(|| (k.to_owned(), parse_value(x)))
            })
            .collect();
        return Value::Map(entries);
    }
    if is_number(v)
        && let Ok(n) = v.parse::<f64>()
    {
        return Value::Number(n);
    }
    match v {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::Text(strip_quotes(v)),
    }
}

fn is_number(v: &str) -> bool {
    let digits = v.strip_prefix('-').unwrap_or(v);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, "0"));
    !whole.is_empty()
        && !fraction.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.bytes().all(|b| b.is_ascii_digit())
}

/// A frontmatter that could not be read.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrontmatterError {
    #[error("the file must start with a line `---` (the frontmatter); it starts with {0:?}")]
    NoStart(String),
    #[error("missing --- frontmatter ---")]
    Missing,
    #[error("bad frontmatter line: {0}")]
    BadLine(String),
}

/// The frontmatter entries (in order) and the body after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Frontmatter {
    pub entries: Vec<(String, Value)>,
    pub body: String,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// Splits `---\n...\n---\n` from the body and parses its lines.
pub fn parse_frontmatter(text: &str) -> Result<Frontmatter, FrontmatterError> {
    let rest = text.strip_prefix("---").ok_or_else(|| {
        FrontmatterError::NoStart(text.lines().next().unwrap_or("").chars().take(60).collect())
    })?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .ok_or(FrontmatterError::Missing)?;
    let (front, body) = split_at_closing(rest).ok_or(FrontmatterError::Missing)?;
    let mut entries = Vec::new();
    for line in front.lines() {
        let clean = strip_comment(line);
        if clean.trim().is_empty() {
            continue;
        }
        let (key, value) = clean
            .split_once(':')
            .ok_or_else(|| FrontmatterError::BadLine(line.to_owned()))?;
        entries.push((key.trim().to_owned(), parse_value(value)));
    }
    Ok(Frontmatter {
        entries,
        body: body.to_owned(),
    })
}

/// `text` without a code fence wrapped around it (a model writing a card
/// as a Markdown code block: a first line such as ```` ```markdown ```` before
/// the `---`, and a closing ```` ``` ```` line); `None` when it has none.
pub fn unfence(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let (first, rest) = trimmed.split_once('\n')?;
    if !first.trim_end().starts_with("```") || !rest.trim_start().starts_with("---") {
        return None;
    }
    let rest = rest.trim_start();
    let body = rest.trim_end();
    let body = body
        .strip_suffix("```")
        .map_or(body, |b| b.trim_end_matches([' ', '\t']));
    Some(format!("{}\n", body.trim_end()))
}

/// Finds the first line that is exactly `---` and returns what is before and
/// after it (the newline following the marker is consumed).
fn split_at_closing(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

fn strip_comment(line: &str) -> &str {
    // `\s+#.*$`: a comment must be preceded by whitespace.
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && i > 0 && bytes[i - 1].is_ascii_whitespace() {
            return line[..i].trim_end();
        }
    }
    line
}

/// Rewrites or appends `key: value` lines in the frontmatter of `text`,
/// leaving everything else untouched. Values are written verbatim.
pub fn set_fields(text: &str, fields: &[(&str, String)]) -> String {
    let Some(end) = text.get(4..).and_then(|t| t.find("\n---")).map(|at| at + 4) else {
        return text.to_owned();
    };
    let (front, rest) = text.split_at(end);
    let mut front = front.to_owned();
    for (key, value) in fields {
        let line = format!("{key}: {value}");
        let mut replaced = false;
        let lines: Vec<String> = front
            .split('\n')
            .map(|l| {
                if !replaced && l.starts_with(key) && l[key.len()..].starts_with(':') {
                    replaced = true;
                    line.clone()
                } else {
                    l.to_owned()
                }
            })
            .collect();
        front = lines.join("\n");
        if !replaced {
            front.push('\n');
            front.push_str(&line);
        }
    }
    front + rest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars_lists_and_maps() {
        assert_eq!(parse_value(" 42 "), Value::Number(42.0));
        assert_eq!(parse_value("-1.5"), Value::Number(-1.5));
        assert_eq!(parse_value("true"), Value::Bool(true));
        assert_eq!(parse_value("\"quoted\""), Value::Text("quoted".into()));
        assert_eq!(
            parse_value("[R4, 'R9', ]"),
            Value::List(vec!["R4".into(), "R9".into()])
        );
        assert_eq!(
            parse_value("{turns: 40, tokens: 60000}"),
            Value::Map(vec![
                ("turns".into(), Value::Number(40.0)),
                ("tokens".into(), Value::Number(60000.0))
            ])
        );
        assert_eq!(parse_value("[]"), Value::List(vec![]));
        assert_eq!(parse_value("T007"), Value::Text("T007".into()));
        assert_eq!(parse_value("1e3"), Value::Text("1e3".into()));
    }

    #[test]
    fn parses_a_card_frontmatter() {
        let text = "---\nid: T007\nstatus: todo   # orchestrator-owned\nrequires: [R4, R9]\ndepends: []\nbudget: {turns: 40}\n---\n## Goal\nBody\n";
        let fm = parse_frontmatter(text).unwrap();
        assert_eq!(fm.get("id"), Some(&Value::Text("T007".into())));
        assert_eq!(fm.get("status"), Some(&Value::Text("todo".into())));
        assert_eq!(fm.get("depends").unwrap().to_list(), Vec::<String>::new());
        assert_eq!(
            fm.get("budget").unwrap().get("turns"),
            Some(&Value::Number(40.0))
        );
        assert_eq!(fm.body, "## Goal\nBody\n");
    }

    #[test]
    fn a_fenced_card_is_unfenced() {
        let fenced = "```markdown\n---\nid: T001\n---\n## Goal\nOne.\n```\n";
        assert_eq!(
            unfence(fenced).unwrap(),
            "---\nid: T001\n---\n## Goal\nOne.\n"
        );
        assert_eq!(
            unfence("\n```\n---\nid: T001\n---\nbody\n").unwrap(),
            "---\nid: T001\n---\nbody\n"
        );
        assert_eq!(unfence("---\nid: T001\n---\n"), None);
        assert_eq!(unfence("```python\nprint(1)\n```\n"), None);
    }

    #[test]
    fn rejects_missing_or_bad_frontmatter() {
        assert_eq!(
            parse_frontmatter("no front").unwrap_err(),
            FrontmatterError::NoStart("no front".into())
        );
        assert!(
            parse_frontmatter("```markdown\n---\nid: T1\n---\n")
                .unwrap_err()
                .to_string()
                .contains("it starts with \"```markdown\"")
        );
        assert_eq!(
            parse_frontmatter("---\nid: T1\n").unwrap_err(),
            FrontmatterError::Missing
        );
        assert_eq!(
            parse_frontmatter("---\njust words\n---\n").unwrap_err(),
            FrontmatterError::BadLine("just words".into())
        );
    }

    #[test]
    fn crlf_is_accepted() {
        let fm = parse_frontmatter("---\r\nid: T1\r\n---\r\nbody").unwrap();
        assert_eq!(fm.get("id"), Some(&Value::Text("T1".into())));
        assert_eq!(fm.body, "body");
    }

    #[test]
    fn set_fields_replaces_or_appends_without_touching_the_body() {
        let text = "---\nid: T007\nstatus: todo\n---\n## Goal\nstatus: not this\n";
        let out = set_fields(
            text,
            &[("status", "active".into()), ("attempts", "1".into())],
        );
        assert_eq!(
            out,
            "---\nid: T007\nstatus: active\nattempts: 1\n---\n## Goal\nstatus: not this\n"
        );
    }

    #[test]
    fn set_fields_leaves_text_without_frontmatter_alone() {
        assert_eq!(set_fields("plain", &[("a", "b".into())]), "plain");
    }
}
