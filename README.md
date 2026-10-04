![vigie](assets/cover.webp)

`vigie` watches GitLab and GitHub and writes to a file the tickets assigned to you that carry given labels and, on GitLab, a given status. It calls no model and writes nothing to either forge: it only reads, through `glab` and `gh`.

The file is meant to be read by another tool: a script, a dashboard, an agent that picks up the tickets to do.

## Requirements

- For GitLab projects: [`glab`](https://gitlab.com/gitlab-org/cli), logged in to an account that can see them (`glab auth login`), and the ticket status field, available on the paid GitLab tiers.
- For GitHub repositories: [`gh`](https://cli.github.com), logged in to an account that can see them (`gh auth login`).
- One of the two is enough when you only watch one forge.
- Rust, to build it.

## Install

```bash
cargo install --path .
```

### With an agent

Paste this prompt into a coding agent (Claude Code, Codex, opencode). It installs `vigie` and whatever it needs that is missing.

```text
Install vigie from https://github.com/Greg-Klein/vigie on this machine.

1. Read the README of the repository to know what vigie needs.
2. Check for Rust (`cargo --version`). If it is missing, install it with rustup from https://rustup.rs, with the default options.
3. Ask me whether my tickets are on GitLab, GitHub or both. For GitLab, check for glab (`glab --version`) and install it if it is missing, following https://gitlab.com/gitlab-org/cli. For GitHub, check for gh (`gh --version`) and install it if it is missing, following https://cli.github.com. Use the package manager of this system (Homebrew on macOS).
4. Run `cargo install --git https://github.com/Greg-Klein/vigie`.
5. Check that `vigie help` answers. If the command is not found, tell me how to add `~/.cargo/bin` to my PATH, without editing my shell files yourself.
6. Run `glab auth status` or `gh auth status`, whichever I use. If a CLI is not logged in, do not log in for me: tell me to run `glab auth login` or `gh auth login`.

Do not use sudo without asking me first. Do not configure vigie and do not start the watch: finish by telling me what you installed, what was already there, and that the next step is `vigie setup`.
```

## Usage

```bash
vigie setup                          # step-by-step configuration
vigie add <url>                      # watch a GitLab project or a GitHub repository
vigie add <url> --group              # or every project of a GitLab group
vigie remove <url>                   # stop watching it
vigie set label "team-a, frontend"   # required labels
vigie set status "To do"             # required status, on GitLab
vigie set assignee <account>         # someone else than your glab account
vigie set interval 60                # how often the forges are asked, in seconds
vigie set output ~/tickets.json      # file to write
vigie list                           # projects and settings
vigie check --print                  # one pass, writing nothing
vigie start                          # watch in the background
vigie status                         # state of the watch, last write
vigie logs                           # last lines of the log
vigie stop
vigie run                            # watch in the foreground
```

`vigie` alone is `vigie status`, `vigie help` lists the commands.

A project is added by its URL, as copied from the browser or from `git remote`:

```bash
vigie add https://gitlab.com/acme/shop
vigie add https://gitlab.com/groups/acme        # a whole group
vigie add https://github.com/acme/api
vigie add https://github.com/acme               # every repository of an owner
vigie add git@github.com:acme/api.git
```

The forge is read from the host: `github.com` is GitHub, `gitlab.com` is GitLab. Any other host is a GitHub Enterprise one when `gh` is logged in to it, and a GitLab one otherwise. The address of a page of the project (an issue, a board) works too.

A GitLab group is recognised when the URL is its `/groups/...` page or holds a single name. A subgroup written `https://gitlab.com/acme/team` reads like a project: add `--group`.

`--label`, `--status` and `--assignee` on `vigie add` replace the common filter for that project. Adding a project that is already watched replaces its entry. The same path can be watched on both forges, as two entries; `vigie remove` drops both, and also takes the path as `vigie list` shows it.

Out of the box the status is "To do", no label is required and the forges are asked every 60 seconds (10 at least). `vigie check` exits with 1 when a project could not be asked.

## The filter

A ticket is kept when it is open, assigned to you (or to the account given as `assignee`), carries **every** required label and is in the required status.

- Case is ignored for labels and for the status: `team-a` finds "Team-A".
- The status has to match on its whole name: "To do" does not take "To do - QA".
- Several labels are written separated by commas, or by repeating `--label`.
- With no label set, any label does. With no assignee set, it is the account `glab` is logged in with.
- A group is watched with the projects of its subgroups.

### On GitHub

An issue is kept when it is open, assigned to you (or to the `--assignee` of that source) and carries every required label. Pull requests are never listed.

- **There is no status.** A GitHub issue is open or closed, nothing else, so the status of the filter is ignored there and `--status` is refused on a GitHub URL. Use a label to mark the issues that are ready.
- The shared `assignee` is a GitLab account and is not applied to GitHub: with no `--assignee` on the source, it is the account `gh` is logged in with.
- The URL of an owner, a user or an organisation, watches every one of its repositories, through the issue search. One call brings back 1,000 issues at most: past that the source is reported as failed, narrow it to a repository.
- A repository on a GitHub Enterprise host is kept as `host/owner/repo` in the configuration.

## The file

A snapshot of everything that matches the filter at the time of the pass, written whole and then renamed into place, so a reader never sees a half-written file.

```json
{
  "version": 1,
  "generatedAt": "2026-01-15T09:30:00.000Z",
  "tickets": [
    { "url": "https://gitlab.com/acme/shop/-/work_items/101", "title": "Fix the cart total", "source": "acme/shop" },
    { "url": "https://github.com/acme/api/issues/42", "title": "Rate limit the export", "source": "acme/api" }
  ]
}
```

`source` is the watched project or group the ticket was found through. A ticket found through two of them is written once.

A ticket that no longer matches leaves the file at the next pass. A project that cannot be asked keeps the tickets it had in the previous file.

The file holds ticket titles: keep it out of any repository.

## How it runs

On macOS, `vigie start` installs a `launchd` job that runs one pass per interval: nothing stays in memory between two passes. `vigie stop` removes it. Elsewhere, or with `vigie start --resident`, a process stays open and sleeps between passes (under 2 MB).

The configuration is read again at every pass: a project added while the watch runs is watched at the next one. Changing the interval restarts the `launchd` job.

The log is cut back to its last half past 256 KB.

The configuration, the log and the pid live in `~/.config/vigie/` (`XDG_CONFIG_HOME` is followed, `VIGIE_HOME` names another directory). With no `output` setting, the file is written there as `tickets.json`.

## License

MIT
