mod cli;
mod commands;
mod config;
mod display;
mod format;
mod github;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let crab = {
        let mut builder = octocrab::OctocrabBuilder::default();
        if let Ok(t) = std::env::var("GITHUB_TOKEN")
            && !t.is_empty()
        {
            builder = builder.personal_token(t);
        }
        builder.build()?
    };

    match &cli.command {
        Some(Commands::List) => commands::list::run()?,
        Some(Commands::Rm) => commands::remove::run()?,
        Some(Commands::Add { user }) => commands::add::run(&crab, user.clone()).await?,
        None => commands::digest::run(&crab).await?,
    }

    Ok(())
}
