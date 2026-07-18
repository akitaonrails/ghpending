# GitLab support

Tracks issues and merge requests from GitLab (SaaS or self-hosted) in the same
digest as GitHub repos. See `architecture.md` for how it plugs in.

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

## Out of scope for this MVP

- `ghpending add --gitlab` — interactive project picking. For now, add projects
  by editing the config file. Planned as a follow-up.
- `ghpending list` / `rm` still operate on GitHub repos only.
- Only one GitLab instance at a time.
