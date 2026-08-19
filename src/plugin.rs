use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PluginMetadata {
    pub schema_version: &'static str,
    pub vendor: &'static str,
    pub version: &'static str,
    pub short_description: &'static str,
}

/// Docker's plugin *protocol* version. Unrelated to the crate version, which is
/// taken from Cargo.toml so it cannot drift.
pub const METADATA: PluginMetadata = PluginMetadata {
    schema_version: "0.1.0",
    vendor: "todor",
    version: env!("CARGO_PKG_VERSION"),
    short_description: env!("CARGO_PKG_DESCRIPTION"),
};

pub fn print_metadata() {
    println!(
        "{}",
        serde_json::to_string(&METADATA).expect("metadata serialization")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a top-level `key = "value"` out of Cargo.toml.
    fn manifest_field(key: &str) -> String {
        include_str!("../Cargo.toml")
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{} = \"", key)))
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or_else(|| panic!("no `{}` in Cargo.toml", key))
            .to_string()
    }

    #[test]
    fn version_tracks_the_manifest() {
        // Compared against the manifest text, not against the same env! macro
        // the constant is defined with — otherwise this asserts nothing and a
        // hardcoded literal (the original bug) slips straight through.
        assert_eq!(METADATA.version, manifest_field("version"));
    }

    #[test]
    fn description_tracks_the_manifest() {
        assert_eq!(METADATA.short_description, manifest_field("description"));
    }

    #[test]
    fn schema_version_is_the_docker_protocol_version() {
        // Docker's plugin protocol version, deliberately independent of ours.
        assert_eq!(METADATA.schema_version, "0.1.0");
    }

    #[test]
    fn metadata_is_valid_json_with_the_fields_docker_expects() {
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&METADATA).unwrap()).unwrap();
        for key in ["SchemaVersion", "Vendor", "Version", "ShortDescription"] {
            let value = json.get(key).unwrap_or_else(|| panic!("missing {key}"));
            assert!(
                value.as_str().is_some_and(|v| !v.is_empty()),
                "{key} must be a non-empty string, got {value}"
            );
        }
    }
}
