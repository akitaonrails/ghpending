# Architecture

`ghpending` is a single Rust binary that fans out to one or more forges, folds
their answers into one neutral shape, and prints a digest. The layering exists so
that adding a forge does not touch rendering.

## Layers

```
config.rs          what to watch: repos, [gitlab].projects, theme
   │
   ├── github_client.rs      Octocrab, SOCKS/Tor-aware
   └── gitlab_client.rs      hyper + rustls, always direct
   │
   ├── github.rs             GitHub REST  ─┐
   └── gitlab.rs             GitLab API v4 ┤
   │                                       │
model.rs           RepoItem / RepoResult ◄─┘   ← the neutrality seam
   │
commands/digest.rs         schedules fetches, bounded concurrency + timeout
   │
display.rs                 renders; format.rs + theme.rs do text and color
```

Each layer only knows the one below it. `display.rs` imports `model`, never
`github` or `gitlab`.

## The neutrality seam

`src/model.rs` holds `RepoItem`, `ItemKind`, `RepoResult`, `RepoStatus`,
`RepoError` and `item_cmp`. It has no provider-specific field and no dependency
on any HTTP client. A provider module's whole job is to produce `RepoResult`
values; ordering (`item_cmp`: PRs before issues, then number descending),
rendering, the empty-repo skip and the summary line all come for free.

`RepoResult.repo` is a **display label**, not an identifier — that is why GitLab
projects carry `{host}/{path}` (e.g. `gitlab.dpe.br/nucleo-ti/portal`) and
GitHub repos carry the bare `owner/repo`. The provider identity a fetch needs is
kept in `digest::FetchTask`, not smuggled through the label.

## Scheduling

`commands/digest.rs` builds a `Vec<FetchTask>` — one variant per provider — and
drives them through a `FuturesUnordered` with `MAX_CONCURRENT_FETCHES = 4` and a
30s wall-clock budget. Results are written back by index, so digest order always
matches config order regardless of completion order. Anything unfinished when the
budget expires renders as `timeout after 30s`.

That budget is shared by every provider. A single project with thousands of open
items can exhaust it on its own, since both backends paginate fully.

## Adding a provider

1. A client module for transport and auth (see `gitlab_client.rs` — ~150 lines
   of hyper is enough; a heavyweight SDK is not required).
2. A provider module that maps the API onto `model` types and returns
   `RepoResult`, translating "not found" into `RepoStatus::NotFound` and every
   other failure into `RepoStatus::Error`.
3. A config section, a `FetchTask` variant, and a branch in `digest::fetch_one`.

Nothing below the seam should need to change. If it does, the abstraction is
leaking and the fix belongs in the provider module.
