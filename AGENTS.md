# AGENTS.md

## Project shape

- Single Rust CLI crate (`ghpending`, edition 2024), not a workspace. Binary entrypoint is `src/main.rs`; CLI definition is `src/cli.rs`; command implementations are in `src/commands/`; the provider-neutral item model (`RepoItem`/`RepoResult`/`item_cmp`) is in `src/model.rs`; GitHub API/listing logic is in `src/github.rs` (transport in `src/github_client.rs`); GitLab API v4 mapping is in `src/gitlab.rs` (transport in `src/gitlab_client.rs`); rendering is in `src/display.rs`; config persistence is in `src/config.rs`.
- Providers are pluggable behind `src/model.rs`: a backend's only job is to produce `RepoResult` values, and `display.rs` never imports `github`/`gitlab`. See `docs/architecture.md` for the layering and `docs/gitlab.md` for the GitLab decisions.

## Commands

- Baseline verification: `cargo test` (this is what release CI runs before building).
- Interactive prompts are covered by PTY-driven end-to-end tests in `tests/interactive.rs` (`cargo test --test interactive`, ~1s). They spawn the real binary through a pseudo-terminal with `rexpect`, point `XDG_CONFIG_HOME` at a temp dir, and strip ANSI codes before matching. `inquire`'s own mock terminal is `pub(crate)`, so driving a real PTY is the only way to exercise the prompts from outside the crate.
- `add` tests hit a throwaway `TcpListener` serving canned GitLab JSON (`fake_gitlab` in that file) instead of a real instance, so they stay offline and deterministic. Add new provider-listing cases there rather than against a live host.
- Focus one unit test with normal Rust filters, e.g. `cargo test model::tests::item_cmp_sorts_prs_before_issues_then_number_desc` or `cargo test commands::add::tests::flag_overrides_saved_user`.
- Release builds mirror CI: `cargo build --release --target x86_64-unknown-linux-gnu` and `cargo build --release --target aarch64-apple-darwin`. CI installs stable Rust; there is no repo `rust-toolchain` file.
- Manual CLI entrypoints: `cargo run --` for the digest, `cargo run -- add [--user <name>|--all]`, `cargo run -- list`, and `cargo run -- rm`. `add`/`rm` are interactive; `add` and the digest hit the live GitHub API, and the digest hits the live GitLab API when `[gitlab]` is configured.

## Runtime gotchas

- `GITLAB_TOKEN` (env, wins over `[gitlab].token` in the config) authenticates GitLab; the `[gitlab]` section is optional and absent means no GitLab client is built at all. `url` defaults to `https://gitlab.com` when omitted.
- The GitLab client is always direct and deliberately bypasses the SOCKS proxy path, since self-hosted instances are usually internal-network only.
- `GITHUB_TOKEN` is optional for public repos/rate limit, but private repos only show up when the token can read them. Use `NO_COLOR=1` when snapshotting output.
- GitHub API client auto-routes through a SOCKS proxy when one is already available at `127.0.0.1:9050`; `GHPENDING_GITHUB_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY` are also honored for `socks5`/`socks5h` values. If no proxy is available, it falls back to direct API access.
- Config is user-local, not repo-local: Linux `~/.config/ghpending/config.toml`, macOS `~/Library/Application Support/ghpending/config.toml`; saves use mode `0600` on Unix. On Linux, set a temporary `XDG_CONFIG_HOME` for manual runs if you do not want to mutate the real watch list.
- `ghpending add --user <name>` persists/replaces the saved default user. `ghpending add --all` ignores the saved user and lists every token-visible owned/collaborator/org-member repo.
- Bare `ghpending add` prompts for the provider (GitHub/GitLab) first; `--user`/`--all` are GitHub-only and skip that prompt. The GitLab branch builds its own client, because `main` only builds one when `[gitlab]` already exists and `add` is what creates that section.
- Anonymous GitLab `membership=true` listing returns an empty list, not 401, so `add` checks `GitlabClient::has_token()` to explain an empty result instead of dead-ending.
- Listing source behavior is intentional: the authenticated user's own login uses the authenticated repo listing, org targets use org listing, and third-party users are public-only.

## Behavior to preserve

- Digest fetches tracked repos with bounded concurrency (`MAX_CONCURRENT_FETCHES = 4`) and a 30s timeout window; timed-out/unstarted repos render as `timeout after 30s`.
- GitHub items are fetched from issues and pulls separately; PRs duplicated in the issues endpoint are skipped. Sort order is PRs first, then issues, with each group by number descending.
- GitLab maps merge requests onto `ItemKind::PullRequest` and `iid` onto `number`; its issues endpoint does not return MRs, so no dedup filter is needed there. Pagination follows `x-next-page` at `per_page=100`, capped at `MAX_PAGES = 20`.
- GitLab results are labeled `{host}/{path}` (e.g. `gitlab.com/group/app`) to disambiguate them from GitHub's `owner/repo` in a mixed digest. The label is display-only; provider identity lives in `digest::FetchTask`.
- The 30s digest budget is shared across providers. Very large projects (e.g. `gitlab-org/gitlab-runner`, ~2000 open items) can exhaust it alone and render as a timeout; that is expected, not a GitLab bug.
- `list` and `rm` share `commands::list::watch_entries`, which labels GitHub repos as `owner/repo` and GitLab projects as `{host}/{path}`. Each entry carries a `Target` so `rm` never has to parse a label back into a provider; the `Target` holds the bare project path that `[gitlab].projects` stores.
- The digest omits repos with zero open items, but the summary still reports total repos checked and how many have pending tasks.
- `add` stores repos sorted after selection.

## Release and packaging

- `.github/workflows/release.yml` runs on `v*` tags and `workflow_dispatch`; tag builds create GitHub release tarballs, then publish to crates.io, Homebrew tap, and AUR only when the corresponding secrets exist.
- Cargo package publishing excludes `.github/`, `target/`, `.claude/`, `docs/`, and `packaging/` via `Cargo.toml`; do not rely on those files being present in the crates.io package.
- AUR PKGBUILDs in `packaging/aur/` intentionally carry the last released `pkgver`/checksums. The release workflow renders updated copies from the tag, so do not “fix” them just because `Cargo.toml` is newer.
- If editing AUR packaging, run from `packaging/aur`: `makepkg -p PKGBUILD-bin --verifysource` and `makepkg -p PKGBUILD-bin -Ccf`.
