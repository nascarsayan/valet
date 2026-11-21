//! The Slack bot token and DM channel for `valet cc`. Each comes from its
//! environment variable (SLACK_BOT_TOKEN, SLACK_CHANNEL), else the `valet vars`
//! entry of the same name; neither has a default.

use anyhow::{Result, bail};

const TOKEN_ENV: &str = "SLACK_BOT_TOKEN";
const CHANNEL_ENV: &str = "SLACK_CHANNEL";

/// NAME from ENV_VALUE if set and non-empty, else from the vars store
/// (STORED: `None` when the store has no entry). WHAT names it in the error.
/// Errors never include the value.
fn pick(
    name: &str,
    what: &str,
    env_value: Option<String>,
    stored: impl FnOnce() -> Option<Result<String>>,
) -> Result<String> {
    if let Some(v) = env_value.filter(|v| !v.is_empty()) {
        return Ok(v);
    }
    match stored() {
        Some(Ok(v)) if !v.is_empty() => Ok(v),
        Some(Ok(_)) => bail!("no {what}: valet vars entry {name} is empty"),
        Some(Err(e)) => bail!("no {what}: cannot read valet vars entry {name}: {e:#}"),
        None => bail!("no {what}: set {name} in the environment or with `valet vars set {name}`"),
    }
}

fn from_env_or_vars(name: &str, what: &str) -> Result<String> {
    pick(name, what, std::env::var(name).ok(), || {
        crate::cmd::vars::has(name).then(|| crate::cmd::vars::value(name))
    })
}

/// The bot token (`xoxb-...`). Never log it or pass it in argv.
pub fn bot_token() -> Result<String> {
    from_env_or_vars(TOKEN_ENV, "Slack bot token")
}

/// The id of the DM between the bot and the operator.
pub fn default_channel() -> Result<String> {
    from_env_or_vars(CHANNEL_ENV, "Slack DM channel id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_wins_then_vars() {
        let unused = || -> Option<Result<String>> { panic!("store read with env set") };
        assert_eq!(
            pick("X", "x", Some("from-env".into()), unused).unwrap(),
            "from-env"
        );
        assert_eq!(
            pick("X", "x", Some(String::new()), || Some(Ok("stored".into()))).unwrap(),
            "stored"
        );
        assert_eq!(
            pick("X", "x", None, || Some(Ok("stored".into()))).unwrap(),
            "stored"
        );
    }

    #[test]
    fn missing_is_a_clear_error() {
        let msg = pick("SLACK_BOT_TOKEN", "Slack bot token", None, || None)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("SLACK_BOT_TOKEN") && msg.contains("valet vars set"),
            "{msg}"
        );
        let msg = pick("X", "x", None, || Some(Ok(String::new())))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("empty"), "{msg}");
        let msg = pick("X", "x", None, || Some(Err(anyhow::anyhow!("gpg locked"))))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("gpg locked"), "{msg}");
    }
}
