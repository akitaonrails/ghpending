use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ghpending",
    about = "Digest of open issues and PRs across watched repos"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Pick repos from a GitHub user/org to track
    Add {
        /// GitHub user/org to list repos from; replaces the saved one.
        /// Lists private repos too when it's your own account or an org you
        /// belong to (needs a GITHUB_TOKEN with `repo` scope).
        #[arg(long, conflicts_with = "all")]
        user: Option<String>,
        /// List every repo your token can reach (owned, collaborator and
        /// org-member), private included, ignoring the saved user.
        #[arg(long)]
        all: bool,
    },
    /// Remove repos from the watch list
    Rm,
    /// Print all tracked repos
    List,
}
