//! Claude Code user settings valet owns: `claude/settings.json` in the valet
//! repo, merged into the live `settings.json` by `valet hooks install`.
//!
//! Keys in the base are valet's: objects merge key by key, other values
//! replace. Every other key stays as Claude wrote it (permissions, /config
//! choices, the hooks `valet hooks` manages). Placeholders in strings: `{valet}`,
//! the valet checkout (alias `valet`); `{claude}`, Claude's folder
//! (CLAUDE_CONFIG_DIR).

use anyhow::{Context, Result};
use serde_json::Value;

const BASE: &str = include_str!("../../claude/settings.json");

/// The base with placeholders filled in.
fn base(checkout: &str, claude: &str) -> Result<Value> {
    let mut v: Value = serde_json::from_str(BASE).context("claude/settings.json")?;
    fill(&mut v, checkout, claude);
    Ok(v)
}

fn fill(v: &mut Value, checkout: &str, claude: &str) {
    match v {
        Value::String(s) => *s = s.replace("{valet}", checkout).replace("{claude}", claude),
        Value::Array(a) => a.iter_mut().for_each(|x| fill(x, checkout, claude)),
        Value::Object(m) => m.values_mut().for_each(|x| fill(x, checkout, claude)),
        _ => {}
    }
}

/// Merge BASE into LIVE; the dotted paths of the values that changed.
fn merge(live: &mut Value, base: &Value, at: &str, changed: &mut Vec<String>) {
    match (live, base) {
        (Value::Object(l), Value::Object(b)) => {
            for (k, bv) in b {
                let path = if at.is_empty() {
                    k.clone()
                } else {
                    format!("{at}.{k}")
                };
                match l.get_mut(k) {
                    Some(lv) if lv.is_object() && bv.is_object() => merge(lv, bv, &path, changed),
                    Some(lv) if lv == bv => {}
                    _ => {
                        l.insert(k.clone(), bv.clone());
                        changed.push(path);
                    }
                }
            }
        }
        (l, b) => {
            if l != b {
                *l = b.clone();
                changed.push(at.to_owned());
            }
        }
    }
}

/// Settings TEXT with the base merged in, and what changed; `None` when
/// nothing would.
pub fn apply(text: &str, checkout: &str, claude: &str) -> Result<Option<(String, Vec<String>)>> {
    let mut live: Value = serde_json::from_str(text).context("parse Claude settings.json")?;
    let mut changed = Vec::new();
    merge(&mut live, &base(checkout, claude)?, "", &mut changed);
    if changed.is_empty() {
        return Ok(None);
    }
    Ok(Some((serde_json::to_string_pretty(&live)? + "\n", changed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base_parses() {
        let b = base("/d", "/c").unwrap();
        assert!(b.is_object());
        assert_eq!(b["env"]["CLAUDE_CODE_SHELL"], "/bin/bash");
    }

    #[test]
    fn placeholders_are_filled_everywhere() {
        let mut v = json!({"a": "{valet}/x", "b": ["{claude}/y", 1], "c": {"d": "{valet}"}});
        fill(&mut v, "/d", "/c");
        assert_eq!(v, json!({"a": "/d/x", "b": ["/c/y", 1], "c": {"d": "/d"}}));
    }

    #[test]
    fn merges_owned_keys_and_keeps_the_rest() {
        let live = r#"{"permissions":{"allow":["Bash(ls)"]},"env":{"FOO":"1","CLAUDE_CODE_SHELL":"/bin/zsh"}}"#;
        let (text, changed) = apply(live, "/d", "/c").unwrap().unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls)");
        assert_eq!(v["env"]["FOO"], "1");
        assert_eq!(v["env"]["CLAUDE_CODE_SHELL"], "/bin/bash");
        assert_eq!(changed, ["env.CLAUDE_CODE_SHELL"]);
        assert!(apply(&text, "/d", "/c").unwrap().is_none(), "idempotent");
    }

    #[test]
    fn nested_objects_merge_key_by_key() {
        let mut live = json!({"m": {"keep": 1, "set": {"x": 1}}});
        let mut changed = Vec::new();
        merge(
            &mut live,
            &json!({"m": {"set": {"x": 2}, "new": true}}),
            "",
            &mut changed,
        );
        assert_eq!(
            live,
            json!({"m": {"keep": 1, "set": {"x": 2}, "new": true}})
        );
        assert_eq!(changed, ["m.new", "m.set.x"]);
    }
}
