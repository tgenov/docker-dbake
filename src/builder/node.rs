#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub endpoint: String,
    pub platforms: Vec<String>,
}

/// A node paired with its ephemeral shard builder name.
#[derive(Debug, Clone)]
pub struct ShardNode {
    pub name: String,
    pub shard_builder: String,
    pub platforms: Vec<String>,
}
