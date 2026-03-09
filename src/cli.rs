use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "docker-dbake",
    about = "Distributed Docker Bake — work-stealing across buildkitd nodes"
)]
pub struct Cli {
    /// Compose/bake file
    #[arg(short = 'f', long = "file", default_value = "docker-compose.yml")]
    pub file: PathBuf,

    /// Buildx builder name [default: current active builder]
    #[arg(long)]
    pub builder: Option<String>,

    /// Build only services in this compose profile
    #[arg(long)]
    pub profile: Option<String>,

    /// Include depends_on chain for specified targets
    #[arg(long)]
    pub with_deps: bool,

    /// Skip these targets (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub exclude: Vec<String>,

    /// Constrain shard builder platform
    #[arg(long)]
    pub platform: Option<String>,

    /// Registry for type=registry cache (mode=max)
    #[arg(long)]
    pub cache_registry: Option<String>,

    /// Pass --no-cache to bake
    #[arg(long)]
    pub no_cache: bool,

    /// Load images into docker
    #[arg(long)]
    pub load: bool,

    /// Push images to registry
    #[arg(long)]
    pub push: bool,

    /// Progress output: auto (TUI when interactive, plain otherwise) or plain (line-by-line)
    #[arg(long, default_value = "auto", value_parser = ["auto", "plain"])]
    pub progress: String,

    /// Abort all on first failure
    #[arg(long)]
    pub fail_fast: bool,

    /// Target services to build
    pub targets: Vec<String>,
}
