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
        /// GitHub user/org to list repos from; replaces the saved one
        #[arg(long)]
        user: Option<String>,
    },
    /// Remove repos from the watch list
    Rm,
    /// Print all tracked repos
    List,
}
