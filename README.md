# ghpending

See open issues and pull requests across the GitHub repos you care about, at a glance.

![ghpending output](https://raw.githubusercontent.com/akitaonrails/ghpending/main/docs/screenshot.png)

## Install

### Homebrew (macOS / Linux)

```sh
brew tap akitaonrails/tap && brew install ghpending
```

### Cargo

```sh
cargo install ghpending
```

### mise

```sh
mise use -g github:akitaonrails/ghpending
```

### From source

```sh
git clone https://github.com/akitaonrails/ghpending
cargo install --path ghpending
```

## Upgrading

```sh
# Homebrew
brew upgrade ghpending

# Cargo
cargo install ghpending --force

# mise
mise upgrade ghpending

# From source
cd ghpending && git pull && cargo install --path .
```

## Usage

```sh
ghpending add                # pick repos from the saved user/org to track
ghpending add --user <name>  # switch to a different user/org (replaces the saved one)
ghpending        # print the digest
ghpending list   # show tracked repos
ghpending rm     # remove repos from the list
```

- `ghpending add` — prompts for a GitHub username or org, lists their public repos, and lets you select which ones to track. The username is saved so subsequent `add` runs skip the prompt. Pass `--user <name>` to switch to a different user/org without editing the config; it replaces the saved one.
- `ghpending` — fetches all tracked repos concurrently and prints a digest of open issues and pull requests.
- `ghpending list` — prints the repos currently in your watch list.
- `ghpending rm` — opens an interactive menu to select repos to remove from tracking.

## Authentication (optional)

Everything works unauthenticated for public repos, subject to GitHub's default 60 requests/hour rate limit. Set `GITHUB_TOKEN` to raise that to 5,000 requests/hour:

```sh
GITHUB_TOKEN=$(gh auth token) ghpending
```

The token is read silently at startup — no configuration needed.

## Config

The config file lives at:

- Linux: `~/.config/ghpending/config.toml`
- macOS: `~/Library/Application Support/ghpending/config.toml`

Example:

```toml
user = "akitaonrails"
repos = ["ratatui-org/ratatui", "tokio-rs/tokio"]
```

Run `ghpending add --user <name>` to change the `user` field, or edit the file directly to reorder repos.

## License

MIT
