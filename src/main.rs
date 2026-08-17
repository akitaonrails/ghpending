mod cli;
mod commands;
mod config;
mod display;
mod format;
mod github;
mod github_client;
mod sort;
mod theme;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Commands};
use sort::SortMode;
use theme::Theme;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.limit.is_some() && cli.command.is_some() {
        bail!("--limit can only be used when rendering the digest");
    }
    if cli.subscribed && cli.command.is_some() {
        bail!("--subscribed can only be used when rendering the digest");
    }
    if cli.sort.is_some() && cli.command.is_some() {
        bail!("--sort can only be used when rendering the digest");
    }

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

    let sort_name = cli
        .sort
        .as_deref()
        .or(cfg.sort.as_deref())
        .unwrap_or("activity");
    let sort_mode = match SortMode::by_name(sort_name) {
        Some(mode) => mode,
        None => bail!(
            "unknown sort mode: {sort_name} (available: {})",
            sort::SORT_NAMES.join(", ")
        ),
    };

    let crab = github_client::build()?;

    match &cli.command {
        Some(Commands::List) => commands::list::run()?,
        Some(Commands::Rm) => commands::remove::run()?,
        Some(Commands::Add { user, all }) => commands::add::run(&crab, user.clone(), *all).await?,
        None => {
            commands::digest::run(&crab, &resolved_theme, cli.limit, cli.subscribed, sort_mode)
                .await?
        }
    }

    Ok(())
}
