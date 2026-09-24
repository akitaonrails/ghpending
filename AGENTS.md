# AGENTS.md

## Project shape

- Single Rust CLI crate (`ghpending`, edition 2024), not a workspace. Binary entrypoint is `src/main.rs`; CLI definition is `src/cli.rs`; command implementations are in `src/commands/`; GitHub API/listing/sort logic is in `src/github.rs`; rendering is in `src/display.rs`; config persistence is in `src/config.rs`.

## Commands

- Baseline verification: `cargo test` (this is what release CI runs before building).
- Focus one unit test with normal Rust filters, e.g. `cargo test github::tests::item_cmp_sorts_prs_before_issues_then_number_desc` or `cargo test commands::add::tests::flag_overrides_saved_user`.
- Release builds mirror CI: `cargo build --release --target x86_64-unknown-linux-gnu` and `cargo build --release --target aarch64-apple-darwin`. CI installs stable Rust; there is no repo `rust-toolchain` file.
- Manual CLI entrypoints: `cargo run --` for the digest, `cargo run -- add [--user <name>|--all]`, `cargo run -- list`, and `cargo run -- rm`. `add`/`rm` are interactive; `add` and the digest hit the live GitHub API.

## Runtime gotchas

- Auth token resolution order: `$GHPENDING_GITHUB_TOKEN`, then `$GITHUB_TOKEN`, then `gh auth token` (subprocess, silent on failure), then anonymous. A token is optional for public repos/rate limit, but private repos only show up when the token can read them. Use `NO_COLOR=1` when snapshotting output.
- GitHub API client auto-routes through a SOCKS proxy when one is already available at `127.0.0.1:9050`; `GHPENDING_GITHUB_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY` are also honored for `socks5`/`socks5h` values. If no proxy is available, it falls back to direct API access.
- Config is user-local, not repo-local: Linux `~/.config/ghpending/config.toml`, macOS `~/Library/Application Support/ghpending/config.toml`; saves use mode `0600` on Unix. On Linux, set a temporary `XDG_CONFIG_HOME` for manual runs if you do not want to mutate the real watch list.
- `ghpending add --user <name>` persists/replaces the saved default user. `ghpending add --all` ignores the saved user and lists every token-visible owned/collaborator/org-member repo.
- Listing source behavior is intentional: the authenticated user's own login uses the authenticated repo listing, org targets use org listing, and third-party users are public-only.

## Behavior to preserve

- Digest fetches tracked repos with bounded concurrency (`MAX_CONCURRENT_FETCHES = 4`) and a 30s timeout window; timed-out/unstarted repos render as `timeout after 30s`.
- GitHub items are fetched from issues and pulls separately; PRs duplicated in the issues endpoint are skipped. Sort order is PRs first, then issues, with each group by number descending.
- The digest omits repos with zero open items, but the summary still reports total repos checked and how many have pending tasks.
- `add` stores repos sorted after selection.

## Release and packaging

- `.github/workflows/release.yml` runs on `v*` tags and `workflow_dispatch`; tag builds create GitHub release tarballs, then publish to crates.io, Homebrew tap, and AUR only when the corresponding secrets exist.
- Cargo package publishing excludes `.github/`, `target/`, `.claude/`, `docs/`, and `packaging/` via `Cargo.toml`; do not rely on those files being present in the crates.io package.
- AUR PKGBUILDs in `packaging/aur/` intentionally carry the last released `pkgver`/checksums. The release workflow renders updated copies from the tag, so do not “fix” them just because `Cargo.toml` is newer.
- If editing AUR packaging, run from `packaging/aur`: `makepkg -p PKGBUILD-bin --verifysource` and `makepkg -p PKGBUILD-bin -Ccf`.
