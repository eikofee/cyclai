//! Which plugins CyclAI knows about. They are declared in the environment (so
//! adding one is a change to the docker-compose, nothing is typed in the web
//! UI), as `CYCLAI_PLUGINS=garmin=http://garmin:8080,other=http://other:8080`,
//! each with its own secret in `CYCLAI_PLUGIN_<ID>_SECRET`.

use std::sync::RwLock;

#[derive(Clone, Debug, PartialEq)]
pub struct PluginEndpoint {
    pub id: String,
    pub url: String,
    pub secret: String,
}

static REGISTRY: RwLock<Vec<PluginEndpoint>> = RwLock::new(Vec::new());

/// The registry is process-wide: tests that set it take turns.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn set(endpoints: Vec<PluginEndpoint>) {
    *REGISTRY.write().unwrap() = endpoints;
}

pub fn ids() -> Vec<String> {
    REGISTRY.read().unwrap().iter().map(|e| e.id.clone()).collect()
}

pub fn get(id: &str) -> Option<PluginEndpoint> {
    REGISTRY.read().unwrap().iter().find(|e| e.id == id).cloned()
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Parses `id=url,id=url`; `secret_of` gives the secret of a plugin id.
pub fn parse(spec: &str, secret_of: impl Fn(&str) -> Option<String>) -> Result<Vec<PluginEndpoint>, String> {
    let mut out: Vec<PluginEndpoint> = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (id, url) = part.split_once('=').ok_or_else(|| format!("`{part}`: expected id=url"))?;
        let (id, url) = (id.trim(), url.trim());
        if !valid_id(id) {
            return Err(format!("`{id}`: a plugin id is 1-32 lowercase letters or digits"));
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!("`{id}`: the URL must start with http:// or https://"));
        }
        if out.iter().any(|e| e.id == id) {
            return Err(format!("`{id}` is declared twice"));
        }
        out.push(PluginEndpoint { id: id.to_string(), url: url.trim_end_matches('/').to_string(), secret: secret_of(id).unwrap_or_default() });
    }
    Ok(out)
}

/// Reads `CYCLAI_PLUGINS` and the `CYCLAI_PLUGIN_<ID>_SECRET` variables.
pub fn from_env() -> Result<Vec<PluginEndpoint>, String> {
    let spec = std::env::var("CYCLAI_PLUGINS").unwrap_or_default();
    parse(&spec, |id| std::env::var(format!("CYCLAI_PLUGIN_{}_SECRET", id.to_ascii_uppercase())).ok().map(|s| s.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_are_parsed_with_their_own_secret() {
        let secrets = |id: &str| (id == "garmin").then(|| "s3cret".to_string());
        let eps = parse(" alpha=http://alpha:8080/ , garmin=https://g.example:9000 ", secrets).unwrap();
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0], PluginEndpoint { id: "alpha".into(), url: "http://alpha:8080".into(), secret: String::new() });
        assert_eq!(eps[1].secret, "s3cret");
        assert!(parse("", secrets).unwrap().is_empty());
    }

    #[test]
    fn malformed_declarations_are_rejected() {
        let none = |_: &str| None;
        assert!(parse("alpha", none).is_err());
        assert!(parse("Alpha=http://x", none).is_err(), "uppercase id");
        assert!(parse("a b=http://x", none).is_err());
        assert!(parse("alpha=ftp://x", none).is_err());
        assert!(parse("a=http://x,a=http://y", none).is_err(), "duplicate");
    }
}
