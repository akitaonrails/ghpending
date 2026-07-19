# GitLab support

Tracks issues and merge requests from GitLab (SaaS or self-hosted) in the same
digest as GitHub repos. See `architecture.md` for how it plugs in.

## Adding projects

`ghpending add` asks which provider you are adding from, then runs the matching
flow. `--user` and `--all` are GitHub-only flags, so passing either one settles
the provider and skips that question.

The GitLab branch builds **its own client** rather than receiving one from
`main`. That is deliberate: `main` only builds a client when `[gitlab]` already
exists, so an `add` that depended on it could never create the section in the
first place. When the section is missing, `add` prompts for the instance URL
(defaulting to `https://gitlab.com`) and writes it.

The group prompt drives which endpoint is used:

- blank → `/projects?membership=true` — everything you are a member of
- a group path → `/groups/{group}/projects?include_subgroups=true`

The chosen group is saved as `[gitlab].group` and offered as the default next
time, mirroring how `user` works for GitHub.

An anonymous `membership=true` call returns an **empty list rather than 401**,
so `add` checks whether a token was resolved and says so explicitly instead of
reporting a bare "no projects found".

## Config

```toml
user = "akita"
repos = ["tokio-rs/tokio"]        # GitHub, untouched

[gitlab]
projects = ["gitlab-org/gitlab-runner"]
```

Pointing at a self-hosted instance:

```toml
[gitlab]
url = "https://gitlab.dpe.br"
token = "..."                     # optional; prefer the GITLAB_TOKEN env var
projects = ["nucleo-ti/portal", "grupo/subgrupo/app"]
```

- The `[gitlab]` section is optional. Without it no GitLab client is built and
  nothing about existing configs changes — there is no migration.
- `url` omitted → `https://gitlab.com`. Only a self-hosted user needs to set it.
- Token resolution: `GITLAB_TOKEN` first, then `[gitlab].token`. Blank values
  count as absent. Public projects work unauthenticated.
- `projects` are full paths including subgroups. They are percent-encoded before
  hitting the API, so `grupo/subgrupo/app` becomes `grupo%2Fsubgrupo%2Fapp`.
- `group` is bookkeeping for `add`: the last group you listed from, reused as the
  prompt default. It does not affect the digest.

## Field mapping

| Concept        | GitHub                  | GitLab                        |
| -------------- | ----------------------- | ----------------------------- |
| change request | pull request            | merge request (rendered `PR`) |
| number         | `number`                | `iid` (per-project)           |
| open state     | `state=open`            | `state=opened`                |
| author         | `user.login`            | `author.username`             |
| draft          | `draft`                 | `draft`                       |
| digest label   | `owner/repo`            | `{host}/{path}`               |

Two details differ from GitHub in ways worth remembering:

- GitLab's issues endpoint never returns merge requests, so the "skip PRs that
  appear as issues" filter the GitHub path needs has no counterpart here.
- `iid` is per-project, which is what we want — it is the number shown in the
  GitLab UI. The global `id` is deliberately ignored.
- `author` can be `null` for items whose account was deleted; those render as
  `unknown` rather than failing the whole project.

## Networking

The GitLab client is **always direct** — it deliberately does not use the
SOCKS/Tor proxy path that `github_client.rs` auto-detects. A self-hosted
instance is usually reachable only on an internal network, so routing it through
Tor would break it.

It is a small hyper + rustls client (native root certs, 10s connect / 30s read,
mirroring the GitHub timeouts) rather than a GitLab SDK; the API surface used is
two endpoints.

## Pagination

Both endpoints are paged at `per_page=100`, following the `x-next-page` response
header until it comes back empty, capped at `MAX_PAGES = 20` (2000 items per
endpoint) so a misbehaving server cannot loop forever.

Note that projects with thousands of open items can exceed the digest's shared
30s budget and render as `timeout after 30s` — `gitlab-org/gitlab-runner`
(~2000 open items, ~33s) is one such project. This is not GitLab-specific; a
GitHub repo of the same size behaves the same way.

## `list` and `rm`

Both cover the two providers. The shared `watch_entries` in
`src/commands/list.rs` builds the labeled list once, so `list` and `rm` can
never drift apart.

Each entry carries a `Target` (`Github(repo)` or `Gitlab(path)`) alongside its
display label. `rm` reads that tag instead of parsing the label back apart —
the label is `{host}/{path}` for GitLab and `owner/repo` for GitHub, and
recovering the provider from that string would be guesswork. The `Target` also
holds the **bare** project path, which is what gets removed from
`[gitlab].projects`.

## Out of scope

- Only one GitLab instance at a time.
- `add` cannot move a project between instances; change `url` by hand.
