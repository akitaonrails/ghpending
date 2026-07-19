//! End-to-end tests that drive the real binary through a pseudo-terminal.
//!
//! `inquire`'s own mock terminal is `pub(crate)`, so the prompts can only be
//! exercised by acting like a user in front of a TTY. `add` is pointed at a
//! throwaway HTTP server instead of a real GitLab so the runs stay offline and
//! deterministic.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;

use rexpect::reader::Options;
use rexpect::session::PtySession;
use rexpect::spawn_with_options;

const TIMEOUT_MS: u64 = 15_000;

/// A config dir under a unique temp path, so tests never touch a real one and
/// never collide with each other.
fn config_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ghpending-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ghpending")).unwrap();
    dir
}

fn write_config(dir: &Path, contents: &str) {
    std::fs::write(dir.join("ghpending/config.toml"), contents).unwrap();
}

fn read_config(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("ghpending/config.toml")).unwrap_or_default()
}

/// Spawns the binary under a PTY with `XDG_CONFIG_HOME` pointed at `dir` and no
/// ambient tokens, so a developer's real credentials cannot leak into a run.
fn spawn_ghpending(dir: &Path, args: &[&str]) -> PtySession {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ghpending"));
    cmd.args(args)
        .env("XDG_CONFIG_HOME", dir)
        .env("NO_COLOR", "1")
        .env_remove("GITLAB_TOKEN")
        .env_remove("GITHUB_TOKEN");

    spawn_with_options(
        cmd,
        Options::new()
            .timeout_ms(Some(TIMEOUT_MS))
            // inquire redraws with ANSI control codes; matching raw output is
            // unreadable and brittle.
            .strip_ansi_escape_codes(true),
    )
    .unwrap()
}

/// A one-shot GitLab stand-in: answers every request with `body` and reports no
/// further pages. Returns the base URL to put in `[gitlab].url`.
fn fake_gitlab(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            serve_once(stream, body);
        }
    });

    format!("http://{addr}")
}

fn serve_once(mut stream: TcpStream, body: &'static str) {
    // Drain the request head; the client sends no body.
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        if line == "\r\n" || line == "\n" {
            break;
        }
        line.clear();
    }

    // An empty `x-next-page` is how GitLab signals the last page.
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         x-next-page: \r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

#[test]
fn rm_removes_only_the_selected_gitlab_project() {
    let dir = config_dir("rm");
    write_config(
        &dir,
        r#"repos = ["owner/repo", "foo/bar"]

[gitlab]
url = "https://gitlab.example.org"
projects = ["grupo/backend", "grupo/infra"]
"#,
    );

    let mut p = spawn_ghpending(&dir, &["rm"]);
    p.exp_string("Select repos to remove:").unwrap();

    // Entries are GitHub-first, so two downs land on the first GitLab project.
    p.send("\x1b[B").unwrap();
    p.send("\x1b[B").unwrap();
    p.send(" ").unwrap();
    p.send("\r").unwrap();
    p.flush().unwrap();
    p.exp_string("Removed 1 repo(s).").unwrap();
    p.exp_eof().unwrap();

    let cfg = read_config(&dir);
    // The GitLab project is gone and the GitHub repos are untouched — the whole
    // point of `rm` carrying a provider tag rather than parsing labels.
    assert!(cfg.contains(r#""owner/repo""#), "github repos lost: {cfg}");
    assert!(cfg.contains(r#""foo/bar""#), "github repos lost: {cfg}");
    assert!(!cfg.contains("grupo/backend"), "not removed: {cfg}");
    assert!(cfg.contains("grupo/infra"), "wrong project removed: {cfg}");
}

#[test]
fn rm_removes_a_github_repo_without_touching_gitlab() {
    let dir = config_dir("rm-gh");
    write_config(
        &dir,
        r#"repos = ["owner/repo"]

[gitlab]
url = "https://gitlab.example.org"
projects = ["grupo/backend"]
"#,
    );

    let mut p = spawn_ghpending(&dir, &["rm"]);
    p.exp_string("Select repos to remove:").unwrap();
    p.send(" ").unwrap();
    p.send("\r").unwrap();
    p.flush().unwrap();
    p.exp_string("Removed 1 repo(s).").unwrap();
    p.exp_eof().unwrap();

    let cfg = read_config(&dir);
    assert!(!cfg.contains("owner/repo"), "not removed: {cfg}");
    assert!(cfg.contains("grupo/backend"), "gitlab project lost: {cfg}");
}

#[test]
fn add_gitlab_bootstraps_the_section_and_tracks_a_picked_project() {
    let dir = config_dir("add");
    write_config(&dir, "repos = []\n");

    let url = fake_gitlab(
        r#"[{"id":1,"path_with_namespace":"grupo/backend"},
            {"id":2,"path_with_namespace":"grupo/infra"}]"#,
    );

    let mut p = spawn_ghpending(&dir, &["add"]);

    p.exp_string("Add repos from:").unwrap();
    p.send("\x1b[B").unwrap(); // GitHub -> GitLab
    p.send("\r").unwrap();
    p.flush().unwrap();

    p.exp_string("GitLab instance URL:").unwrap();
    p.send_line(&url).unwrap();

    p.exp_string("Group to list projects from").unwrap();
    p.send_line("").unwrap(); // blank -> membership listing

    p.exp_string("Select projects to track:").unwrap();
    p.send(" ").unwrap(); // pick the first project
    p.send("\r").unwrap();
    p.flush().unwrap();

    p.exp_string("Saved. Tracking 1 GitLab project(s) total.")
        .unwrap();
    p.exp_eof().unwrap();

    let cfg = read_config(&dir);
    assert!(cfg.contains("[gitlab]"), "section not created: {cfg}");
    assert!(cfg.contains(&url), "instance url not saved: {cfg}");
    assert!(cfg.contains("grupo/backend"), "project not tracked: {cfg}");
    assert!(
        !cfg.contains("grupo/infra"),
        "unpicked project saved: {cfg}"
    );
}

#[test]
fn add_gitlab_remembers_the_group_for_next_time() {
    let dir = config_dir("add-group");
    write_config(&dir, "repos = []\n");

    let url = fake_gitlab(r#"[{"id":1,"path_with_namespace":"defensoria/solar/backend"}]"#);

    let mut p = spawn_ghpending(&dir, &["add"]);
    p.exp_string("Add repos from:").unwrap();
    p.send("\x1b[B").unwrap();
    p.send("\r").unwrap();
    p.flush().unwrap();
    p.exp_string("GitLab instance URL:").unwrap();
    p.send_line(&url).unwrap();
    p.exp_string("Group to list projects from").unwrap();
    p.send_line("defensoria").unwrap();
    p.exp_string("Select projects to track:").unwrap();
    p.send(" ").unwrap();
    p.send("\r").unwrap();
    p.flush().unwrap();
    p.exp_eof().unwrap();

    let cfg = read_config(&dir);
    assert!(cfg.contains(r#"group = "defensoria""#), "group lost: {cfg}");

    // Second run: the instance is settled, so the URL must not be asked again.
    let mut p = spawn_ghpending(&dir, &["add"]);
    p.exp_string("Add repos from:").unwrap();
    p.send("\x1b[B").unwrap();
    p.send("\r").unwrap();
    p.flush().unwrap();
    p.exp_string("Group to list projects from").unwrap();
}

#[test]
fn add_gitlab_keeps_the_url_when_the_listing_comes_back_empty() {
    let dir = config_dir("add-empty");
    write_config(&dir, "repos = []\n");

    let url = fake_gitlab("[]");

    let mut p = spawn_ghpending(&dir, &["add"]);
    p.exp_string("Add repos from:").unwrap();
    p.send("\x1b[B").unwrap();
    p.send("\r").unwrap();
    p.flush().unwrap();
    p.exp_string("GitLab instance URL:").unwrap();
    p.send_line(&url).unwrap();
    p.exp_string("Group to list projects from").unwrap();
    p.send_line("").unwrap();

    // An anonymous membership listing is empty rather than a 401, so the
    // message has to name the likely cause itself.
    p.exp_string("Set GITLAB_TOKEN").unwrap();
    p.exp_eof().unwrap();

    // The URL just typed survives the empty result — no retyping next run.
    let cfg = read_config(&dir);
    assert!(cfg.contains(&url), "instance url discarded: {cfg}");
}

// Note: the `--user`/`--all` skip is covered by the `provider_from_flags` unit
// test in `commands::add`. Asserting it here would mean waiting for the prompt
// *not* to appear, which passes for any unrelated failure and costs a full
// timeout — a worse test than the one that already exists.
