# claude-session-sync

Keep your Claude Code sessions in the Claude desktop app when you switch accounts.

The desktop app keeps a separate Code-tab session list for every account you sign in
with. Sign out, sign in with another account, and every session you had open is gone
from the sidebar. The conversations are still on disk; the app just no longer lists
them. `claude-session-sync` runs in the background and keeps the list the same under
every account on the PC, so after a switch your sessions are there and you can carry on.

- Windows 10/11, one small program, no runtime to install
- Never touches transcripts or sign-in data
- Keeps a copy of anything it replaces (for 14 days), and can undo what it did

> Unofficial. Not made or endorsed by Anthropic. It relies on how the desktop app
> stores its session list today, which is not documented and can change with any app
> update. Read [Limits](#limits) before you rely on it.

## Install

Paste this into PowerShell:

```powershell
irm https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.ps1 | iex
```

It downloads the latest release, checks its SHA-256, and installs it. No administrator
rights, nothing else to install. Then **quit the Claude app completely (tray icon
included) and open it again**: the app reads its session list only when it starts or
signs in, so that is when the sessions from your other accounts appear. From then on,
just switch accounts.

What the installer does, so you know:

- puts the two programs in `%USERPROFILE%\.claude-session-sync\bin` and adds that
  folder to your user `PATH` (new terminals can run `claude-session-sync`);
- registers a scheduled task named `Claude Session Sync` that starts the daemon when
  you log on and restarts it if it ever stops;
- starts the daemon and checks that it really came up.

**Update:** run the same line again; your settings are kept.
**Remove:** `claude-session-sync uninstall`.

To install with options (see [Configuration](#configuration)), pass them like this:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.ps1))) --delete-sync
```

If you already used several accounts before installing, `status` may list some
*conflicts*: sessions whose copies differ in a way the tool will not decide for you.
That is expected. See [Conflicts](#what-it-will-and-will-not-do); `claude-session-sync
resolve --all` settles them in one go.

Prefer to look first? Download `claude-session-sync-windows-x64.zip` from the
[latest release](../../releases/latest), unzip it, and run
`.\claude-session-sync.exe preview` (writes nothing), then `.\claude-session-sync.exe install`.

Notes:

- Each account has to have been signed in on this PC at least once, so its folder exists.
- You can run the installer from a terminal inside the Claude app (or ask Claude Code
  to do it). The app sandboxes what its terminals write under `AppData` and the
  registry; the installer notices and works around it.
- Windows may warn about an unsigned program the first time. Releases are built from
  this repository by GitHub Actions; you can also [build it yourself](#build-from-source).

## Everyday use

There is nothing to do. To see how it is doing:

```
claude-session-sync status
```

```
OK  daemon 0.1.0 running | 91 of 102 sessions in every account | 2 conflicts | 0 errors
autostart: installed (scheduled task "Claude Session Sync")
...
```

The first line is the summary (`OK`, `PAUSED`, `STALLED` or `NOT RUNNING`); below it are
the details: what is pending, anything left alone on purpose, and the last log lines.

| I want to | Command |
|---|---|
| See that it is working | `claude-session-sync status` |
| See what it did lately | `claude-session-sync log` (`-n 100` for more) |
| See what it would change, without changing it | `claude-session-sync preview --verbose` |
| Switch it off for a while, and back on | `claude-session-sync pause` / `resume` |
| Settle a conflict that `status` reports | `claude-session-sync resolve <id>` |
| Get back a session that was removed everywhere | `claude-session-sync restore <id>` |
| Take back what it did since some time | `claude-session-sync undo --since "2026-01-31 14:00"` |
| Change its settings | `claude-session-sync install --delete-sync` (run install again with the options you want) |
| Update it | run the install line again |
| Remove it | `claude-session-sync uninstall` (add `--purge` to delete its files too) |

`<id>` is the session id that `status` prints, or its first six characters.
`claude-session-sync --help` has the full list.

## How it works

The app stores one small record per session:

```
%APPDATA%\Claude\claude-code-sessions\<account>\<organization>\local_<id>.json
```

Transcripts live elsewhere (`~\.claude\projects`) and are not tied to an account. So a
session "disappears" only because its record sits in another account's folder. The
daemon copies each record, byte for byte, into the folder of every account, and keeps
the copies up to date:

- a full pass every hour;
- a cheap check every 10 seconds (it only looks at file names and times), which places
  new sessions at once and refreshes existing copies at most once a minute;
- an immediate pass the moment the app signs out or in, quits or starts, so that
  everything is current before the next account loads its list.

It works out which folder the app has loaded from the app's own log, and never replaces
a record in that folder while the app is running (the app would only write its
in-memory copy back). Those updates wait until you sign out or quit.

## What it will and will not do

**Which copy wins.** The one with the most recent activity. File times alone are not
trusted, because the app re-saves records without any activity.

**Conflicts.** If the copies differ in something *you* decided (archived or not, the
title) and the tool cannot tell which came later, it changes nothing and reports a
conflict in `status`. Both copies stay. A conflict ends by itself when you use the
session again, or with `claude-session-sync resolve <id>` (or `resolve --all`), which
puts the most advanced copy everywhere and keeps the replaced one.

**Deleted sessions.** A session you delete in an account is never put back there, and
is not handed to accounts that never had it. By default it stays in the other accounts
that already have it. Install with `--delete-sync` to have a deletion carried to every
account: the copies are moved to a quarantine folder, not erased, `status` lists them
for a week, and `restore <id>` brings one back everywhere.

**Archived sessions.** Mirrored as archived. Archived sessions unused for more than 30
days are not handed to other accounts (`--days N` changes that); sessions that are not
archived always are.

**Not synced.** Connectors, sidebar groups and pins, scheduled tasks, and anything that
lives on Anthropic's servers stay per account. Sessions whose transcript file is gone
are skipped, since they could not be resumed anyway.

## Safety

- Before it replaces a version it did not write itself, it keeps that version
  (`replaced\`, 14 days, up to 200 MB).
- Before the first change of each day it zips the whole session store (`backups\`; the
  first backup ever is kept for good). If the backup cannot be made, nothing is written.
- Every addition, replacement and removal is written to a journal before it happens.
  (Refreshing a copy the tool placed itself is not: nothing of yours is in it.)
  `undo --since` takes out what was added, puts back what was replaced, and returns what
  was moved to quarantine. Files the app has changed since are left alone. Undo reaches
  back as far as the kept versions do: 14 days.
- A record it cannot make sense of (a newer format, a locked file, anything over 10 MB)
  is neither copied nor replaced.
- Writes are atomic, and re-checked at the last instant: if a file appeared or changed
  in that moment, it is left alone and looked at again later.
- It does not link folders together. Current app builds refuse to save into a linked
  folder, which silently loses new sessions.
- It never uses the network.

Its own files are in `%USERPROFILE%\.claude-session-sync`: `sync.log`, `journal.jsonl`,
`backups\`, `replaced\`, `quarantine\`, and the programs in `bin\`.

## Undo and uninstall

```
claude-session-sync uninstall
```

removes the scheduled task and the `PATH` entry and stops the daemon. The copies it made
stay where they are (your accounts keep the full list), and so do its backups and log.
`claude-session-sync uninstall --purge` also deletes the tool's own folder (the programs
themselves a few seconds later, since a running program cannot delete itself; if
anything is left, it tells you what).

To also take back what it did, quit the Claude app completely and run:

```
claude-session-sync undo --since "2026-01-31 14:00"
```

To go back to the state before you ever ran it: quit the app, empty
`%APPDATA%\Claude\claude-code-sessions`, and unzip the oldest file in `backups\` there.
Mind that this also takes sessions you started since then off the list (their
transcripts stay on disk); `undo` is the gentler way.

## Limits

- **Windows only.** macOS and Linux keep the same kind of records in other places; the
  code is not written for them yet.
- **Undocumented internals.** Tested against the Microsoft Store build of the desktop
  app (5.1231). If an update changes the record format or the log lines this tool reads,
  it becomes more cautious rather than more daring: records it does not understand are
  left alone, and if it cannot tell which account is loaded it stops replacing existing
  records. It may still stop being useful until it is updated.
- **A few seconds around a switch.** A session saved in the last second or two before
  you sign out reaches the other accounts 2 to 5 seconds later. Sign in to the next
  account faster than that and you may see that one session in its previous state;
  using it there makes that copy the current one (the replaced version is kept).
- **A new account's first sign-in.** The account is recognised after about a minute and
  a half; sign out and in once more to see the list.
- **Session-to-session messages** between sessions that were started under different
  accounts are not known to work.
- **Your accounts, your responsibility.** Whether using several accounts fits
  Anthropic's terms is between you and Anthropic.

## Configuration

Settings are given to `install`. They are remembered, so an update keeps them; to change
one, run `install` again with that option:

```
claude-session-sync install --delete-sync --days 60
claude-session-sync install --no-delete-sync
```

| Option | Default | Meaning |
|---|---|---|
| `--delete-sync` | off | Deleting a session in one account takes it out of the others too (to quarantine). |
| `--days N` | 30 | Archived sessions are handed to other accounts only if used in the last N days. |
| `--quick N` | 10 | Seconds between the cheap change checks. `0` = hourly passes only. |
| `--interval N` | 3600 | Seconds between full passes. |

Environment variables, for unusual setups (set them as user variables so the daemon
sees them too):

| Variable | Meaning |
|---|---|
| `CLAUDE_SESSION_SYNC_HOME` | Where the tool keeps its own files. |
| `CLAUDE_SESSION_SYNC_STORE` | The app's `claude-code-sessions` folder, if it is not found. |
| `CLAUDE_CONFIG_DIR` | Honoured as Claude Code does: transcripts are in `<dir>\projects`. |

## Build from source

```
git clone https://github.com/ssgardaf/claude-session-sync
cd claude-session-sync
cargo test --release
cargo build --release
```

Rust 1.82 or later. Every dependency is pure Rust, so both the MSVC and the GNU
toolchain work. The two programs are in `target\release`: `claude-session-sync.exe` is
the command-line tool, `claude-session-syncw.exe` is the same program without a console
window, which the scheduled task runs.

The test suite (more than 200 tests) works in throwaway folders and never writes to your
real profile. A few tests read the process list and the task list.

## License

MIT. See [LICENSE](LICENSE).
