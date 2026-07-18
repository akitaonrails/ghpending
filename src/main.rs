mod cli;
mod commands;
mod config;
mod display;
mod format;
mod github;
mod github_client;
mod gitlab;
mod gitlab_client;
mod model;
mod theme;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Commands};
use theme::Theme;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let cfg = config::load()?;

    let env_specific = std::env::var("GHPENDING_THEME").ok();
    let env_generic = std::env::var("TCLOCK_WIDGET_THEME").ok();
    let theme_name = theme::resolve_name(
        cli.theme.as_deref(),
        env_specific.as_deref(),
        env_generic.as_deref(),
        cfg.theme.as_deref(),
    );
    let resolved_theme = match Theme::by_name(&theme_name) {
        Some(t) => t,
        None => bail!(
            "unknown theme: {} (available: {})",
            theme_name,
            theme::THEME_NAMES.join(", ")
        ),
    };

    let crab = github_client::build()?;
    // Built only when the user configured a `[gitlab]` section.
    let gitlab = match &cfg.gitlab {
        Some(gl_cfg) => Some(gitlab_client::build(gl_cfg)?),
        None => None,
    };

    match &cli.command {
        Some(Commands::List) => commands::list::run()?,
        Some(Commands::Rm) => commands::remove::run()?,
        Some(Commands::Add { user, all }) => commands::add::run(&crab, user.clone(), *all).await?,
        None => commands::digest::run(&crab, gitlab.as_ref(), &resolved_theme).await?,
    }

    Ok(())
}
