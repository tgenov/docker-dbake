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

    /// Also build services gated behind this compose profile (repeatable)
    #[arg(long)]
    pub profile: Vec<String>,

    /// Include depends_on chain for specified targets
    #[arg(long)]
    pub with_deps: bool,

    /// Skip these targets (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub exclude: Vec<String>,

    /// Restrict shards (and scheduling) to these platforms, comma-separated
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

/// How the process was invoked, after applying the Docker CLI plugin protocol.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// `docker-cli-plugin-metadata` — print metadata and exit.
    Metadata,
    /// Normal run; the contained argv is ready for `Cli::parse_from`.
    Run(Vec<String>),
}

/// Apply the Docker CLI plugin protocol to raw argv.
///
/// Docker invokes plugins as `docker-dbake <plugin-name> [args...]`, so the
/// plugin name at argv[1] must be dropped — but only there, otherwise a target
/// that happens to be called `dbake` becomes unbuildable.
pub fn normalize_args(mut args: Vec<String>) -> Invocation {
    // The protocol is positional: docker only ever puts these at argv[1].
    // Matching anywhere would let a target name trigger them.
    if args
        .get(1)
        .is_some_and(|a| a == "docker-cli-plugin-metadata")
    {
        return Invocation::Metadata;
    }
    if args.get(1).is_some_and(|a| a == PLUGIN_NAME) {
        args.remove(1);
    }
    Invocation::Run(args)
}

/// The name docker passes as argv[1] when invoking this plugin.
pub const PLUGIN_NAME: &str = "dbake";

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn strips_the_plugin_name_at_argv1() {
        assert_eq!(
            normalize_args(argv(&["docker-dbake", "dbake", "web"])),
            Invocation::Run(argv(&["docker-dbake", "web"]))
        );
    }

    #[test]
    fn keeps_a_target_named_dbake() {
        // `docker dbake dbake` must build the target called `dbake`.
        assert_eq!(
            normalize_args(argv(&["docker-dbake", "dbake", "dbake"])),
            Invocation::Run(argv(&["docker-dbake", "dbake"]))
        );
    }

    #[test]
    fn keeps_dbake_when_it_is_not_the_plugin_name() {
        // Invoked directly, not through docker: argv[1] is a target.
        assert_eq!(
            normalize_args(argv(&["docker-dbake", "--file", "x.yml", "dbake"])),
            Invocation::Run(argv(&["docker-dbake", "--file", "x.yml", "dbake"]))
        );
    }

    #[test]
    fn detects_metadata_only_at_argv1() {
        assert_eq!(
            normalize_args(argv(&["docker-dbake", "docker-cli-plugin-metadata"])),
            Invocation::Metadata
        );
    }

    #[test]
    fn a_target_named_like_the_metadata_subcommand_is_not_metadata() {
        assert_eq!(
            normalize_args(argv(&[
                "docker-dbake",
                "dbake",
                "docker-cli-plugin-metadata"
            ])),
            Invocation::Run(argv(&["docker-dbake", "docker-cli-plugin-metadata"]))
        );
    }
}
