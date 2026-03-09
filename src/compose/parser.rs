use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Minimal representation of a compose service — only fields needed for profile filtering.
#[derive(Debug, Deserialize)]
pub struct ComposeService {
    #[serde(default)]
    pub profiles: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ComposeFile {
    #[serde(default)]
    pub services: HashMap<String, ComposeService>,
}

/// Parse a compose YAML file for profile metadata.
pub fn parse_compose(path: &Path) -> Result<ComposeFile> {
    let content =
        std::fs::read_to_string(path).context(format!("failed to read {}", path.display()))?;
    let compose: ComposeFile =
        serde_yaml::from_str(&content).context(format!("failed to parse {}", path.display()))?;
    Ok(compose)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_profiles() {
        let yaml = r#"
services:
  web:
    build: .
    profiles:
      - frontend
  api:
    build:
      context: ./api
  db:
    image: postgres
"#;
        let compose: ComposeFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(compose.services["web"].profiles, vec!["frontend"]);
        assert!(compose.services["api"].profiles.is_empty());
    }
}
