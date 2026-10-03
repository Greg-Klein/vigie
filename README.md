# vigie

```
        _       _
 __   _(_) __ _(_) ___
 \ \ / / |/ _` | |/ _ \
  \ V /| | (_| | |  __/
   \_/ |_|\__, |_|\___|
          |___/
```

`vigie` watches GitLab and writes to a file the tickets assigned to you that carry given labels and a given status. It calls no model and writes nothing to GitLab: it only reads, through `glab`.

The file is meant to be read by another tool: a script, a dashboard, an agent that picks up the tickets to do.

## Requirements

- [`glab`](https://gitlab.com/gitlab-org/cli), logged in to an account that can see the projects (`glab auth login`).
- The ticket status field, available on the paid GitLab tiers.
- Rust, to build it.

## Install

```bash
cargo install --path .
```

## Usage

```bash
vigie setup                          # step-by-step configuration
vigie add <group>/<project>          # watch a project
vigie add <group> --group            # or every project of a group
vigie remove <group>/<project>       # stop watching it
vigie set label "team-a, frontend"   # required labels
vigie set status "To do"             # required status
vigie set assignee <account>         # someone else than your glab account
vigie set interval 60                # how often GitLab is asked, in seconds
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

`--label`, `--status` and `--assignee` on `vigie add` replace the common filter for that project. Adding a project that is already watched replaces its entry.

Out of the box the status is "To do", no label is required and GitLab is asked every 60 seconds (10 at least). `vigie check` exits with 1 when a project could not be asked.

## The filter

A ticket is kept when it is open, assigned to you (or to the account given as `assignee`), carries **every** required label and is in the required status.

- Case is ignored for labels and for the status: `team-a` finds "Team-A".
- The status has to match on its whole name: "To do" does not take "To do - QA".
- Several labels are written separated by commas, or by repeating `--label`.
- With no label set, any label does. With no assignee set, it is the account `glab` is logged in with.
- A group is watched with the projects of its subgroups.

## The file

A snapshot of everything that matches the filter at the time of the pass, written whole and then renamed into place, so a reader never sees a half-written file.

```json
{
  "version": 1,
  "generatedAt": "2026-01-15T09:30:00.000Z",
  "tickets": [
    { "url": "https://gitlab.com/acme/shop/-/work_items/101", "title": "Fix the cart total", "source": "acme/shop" }
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
