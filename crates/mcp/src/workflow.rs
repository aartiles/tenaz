//! `ComfyUI` workflow templates: API-format workflow JSON with `{{prompt}}`,
//! `{{negative}}`, `{{seed}}`, `{{width}}` and `{{height}}` placeholders.

use std::collections::BTreeMap;
use std::path::Path;

/// A workflow with placeholders.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowTemplate {
    json: serde_json::Value,
}

impl WorkflowTemplate {
    pub fn new(json: serde_json::Value) -> Self {
        Self { json }
    }

    pub fn from_file(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map(Self::new).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        })
    }

    /// The placeholders the template uses.
    pub fn placeholders(&self) -> Vec<String> {
        let mut found = Vec::new();
        collect(&self.json, &mut found);
        found.sort();
        found.dedup();
        found
    }

    /// Substitutes placeholders. A string that is exactly one placeholder
    /// becomes a number when the value is numeric (seeds, sizes); otherwise
    /// placeholders are replaced inside the text.
    pub fn render(&self, vars: &BTreeMap<&str, String>) -> serde_json::Value {
        substitute(&self.json, vars)
    }
}

fn collect(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(start) = rest.find("{{") {
                let Some(end) = rest[start..].find("}}") else {
                    break;
                };
                out.push(rest[start + 2..start + end].trim().to_owned());
                rest = &rest[start + end + 2..];
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| collect(v, out)),
        _ => {}
    }
}

fn substitute(value: &serde_json::Value, vars: &BTreeMap<&str, String>) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if let Some(name) = trimmed
                .strip_prefix("{{")
                .and_then(|r| r.strip_suffix("}}"))
                .map(str::trim)
                && let Some(replacement) = vars.get(name)
            {
                if let Ok(n) = replacement.parse::<i64>() {
                    return serde_json::Value::from(n);
                }
                if let Ok(n) = replacement.parse::<f64>() {
                    return serde_json::json!(n);
                }
                return serde_json::Value::String(replacement.clone());
            }
            let mut text = s.clone();
            for (name, replacement) in vars {
                text = text.replace(&format!("{{{{{name}}}}}"), replacement);
            }
            serde_json::Value::String(text)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|v| substitute(v, vars)).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_found_and_substituted_with_types() {
        let template = WorkflowTemplate::new(serde_json::json!({
            "3": {"inputs": {"seed": "{{seed}}", "steps": 20, "text": "masterpiece, {{prompt}}", "negative": "{{ negative }}"}},
            "5": {"inputs": {"width": "{{width}}", "height": "{{height}}"}}
        }));
        assert_eq!(
            template.placeholders(),
            vec!["height", "negative", "prompt", "seed", "width"]
        );
        let vars = BTreeMap::from([
            ("prompt", "a cat".to_owned()),
            ("negative", "blurry".to_owned()),
            ("seed", "42".to_owned()),
            ("width", "1024".to_owned()),
        ]);
        let out = template.render(&vars);
        assert_eq!(out["3"]["inputs"]["seed"], 42);
        assert_eq!(out["3"]["inputs"]["text"], "masterpiece, a cat");
        assert_eq!(out["3"]["inputs"]["negative"], "blurry");
        assert_eq!(out["5"]["inputs"]["width"], 1024);
        assert_eq!(
            out["5"]["inputs"]["height"], "{{height}}",
            "an unset placeholder is left alone"
        );
        assert_eq!(out["3"]["inputs"]["steps"], 20);
    }

    #[test]
    fn templates_load_from_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.json");
        std::fs::write(&path, "{\"1\": {\"inputs\": {\"text\": \"{{prompt}}\"}}}").unwrap();
        assert_eq!(
            WorkflowTemplate::from_file(&path).unwrap().placeholders(),
            vec!["prompt"]
        );
        std::fs::write(&path, "nope").unwrap();
        assert!(WorkflowTemplate::from_file(&path).is_err());
    }
}
