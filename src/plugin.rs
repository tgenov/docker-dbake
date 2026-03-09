use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PluginMetadata {
    pub schema_version: &'static str,
    pub vendor: &'static str,
    pub version: &'static str,
    pub short_description: &'static str,
}

pub const METADATA: PluginMetadata = PluginMetadata {
    schema_version: "0.1.0",
    vendor: "todor",
    version: "0.1.0",
    short_description: "Distributed Docker Bake — work-stealing across buildkitd nodes",
};

pub fn print_metadata() {
    println!(
        "{}",
        serde_json::to_string(&METADATA).expect("metadata serialization")
    );
}
