# claude-session-sync

Keep your Claude Code sessions in the Claude desktop app when you switch accounts.

The desktop app keeps a separate Code-tab session list for every account you sign in
with. Sign out, sign in with another account, and every session you had open is gone
from the sidebar. The conversations are still on disk; the app just no longer lists
them. `claude-session-sync` runs in the background and keeps the list the same under
every account on the computer, so after a switch your sessions are there and you can
carry on.

- Windows 10/11 and macOS; Linux is experimental (see [Platforms](#platforms))
- One small program, no runtime to install
- Never touches transcripts or sign-in data
- Keeps a copy of anything it replaces (for 14 days), and can undo what it did

> Unofficial. Not made or endorsed by Anthropic. It relies on how the desktop app
> stores its session list today, which is not documented and can change with any app
> update. Read [Limits](#limits) before you rely on it.

## Install

**Windows.** Paste this into PowerShell:

```powershell
irm https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.ps1 | iex
```

**macOS and Linux.** Paste this into a terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.sh | sh
```

Either one downloads the latest release for your system, checks its SHA-256, and
installs it. No administrator rights (no `sudo`), nothing else to install. Then **quit
the Claude app completely and open it again** (Windows: the tray icon too; macOS:
Cmd+Q): the app reads its session list only when it starts or signs in, so that is when
the sessions from your other accounts appear. From then on, just switch accounts.

What the installer does, so you know:

| | Windows | macOS | Linux |
|---|---|---|---|
| The program | two programs in `%USERPROFILE%\.claude-session-sync\bin` | `~/.claude-session-sync/bin/claude-session-sync` | the same as macOS |
| On your `PATH` | adds that folder to your user `PATH` | links it as `~/.local/bin/claude-session-sync` | the same as macOS |
| Starts by itself | a scheduled task named `Claude Session Sync` | a launch agent, `~/Library/LaunchAgents/io.github.ssgardaf.claude-session-sync.plist` | a systemd user unit, `claude-session-sync.service`, in the folder your systemd reads user units from (`~/.config/systemd/user`, as a rule) |

In every case the daemon starts when you log in and is started again if it ever stops,
and the installer checks that it really came up. Nothing else is changed: on macOS and
Linux no shell profile is edited (if `~/.local/bin` is not on your `PATH`, the installer
says so and shows the full path to use).

**Update:** run the same line again; your settings are kept.
**Remove:** `claude-session-sync uninstall`.

To install with options (see [Configuration](#configuration)), pass them like this:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.ps1))) --delete-sync
```

```sh
curl -fsSL https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.sh | sh -s -- --delete-sync
```

If you already used several accounts before installing, `status` may list some
*conflicts*: sessions whose copies differ in a way the tool will not decide for you.
That is expected. See [Conflicts](#what-it-will-and-will-not-do); `claude-session-sync
resolve --all` settles them in one go.

Prefer to look first? Download the file for your system from the
[latest release](../../releases/latest), unpack it, and run `claude-session-sync preview`
from that folder (it writes nothing), then `claude-session-sync install`:

| System | File |
|---|---|
| Windows (x64) | `claude-session-sync-windows-x64.zip` |
| macOS (Apple silicon and Intel) | `claude-session-sync-macos-universal.tar.gz` |
| Linux (x64 / arm64) | `claude-session-sync-linux-x64.tar.gz` / `claude-session-sync-linux-arm64.tar.gz` |

Notes:

- Each account has to have been signed in on this computer at least once, so its folder
  exists.
- Windows: you can run the installer from a terminal inside the Claude app (or ask
  Claude Code to do it). The app sandboxes what its terminals write under `AppData` and
  the registry; the installer notices and works around it.
- Windows may warn about an unsigned program the first time. macOS shows a "Background
  Items Added" notice for the launch agent, and refuses to run a program that a browser
  downloaded until you allow it (the install line above uses `curl`, which is not
  affected). Releases are built from this repository by GitHub Actions; you can also
  [build it yourself](#build-from-source).
- Linux without a systemd user session (some containers and minimal setups): the
  program is installed, but nothing can start the daemon by itself. `install` says so
  and shows the command to run the daemon yourself, and `status` says so from then on.
- Linux: systemd cannot start a program from a folder with a quote or a backslash in
  its name. `install` says so before it changes anything; keep the tool's folder
  somewhere else (`CLAUDE_SESSION_SYNC_HOME`).

## Everyday use

There is nothing to do. To see how it is doing:

```
claude-session-sync status
```

```
OK  daemon 0.2.0 running | 91 of 102 sessions in every account | 2 conflicts | 0 errors
autostart: installed (scheduled task "Claude Session Sync")
...
```

The first line is the summary (`OK`, `STARTING`, `PAUSED`, `STALLED` or `NOT RUNNING`); below it are
the details: what is pending, anything left alone on purpose, and the last log lines.

| I want to | Command |
|---|---|
| See that it is working | `claude-session-sync status` |
| See what it did lately | `claude-session-sync log` (`-n 100` for more) |
| See what it would change, without changing it | `claude-session-sync preview --verbose` |
| Switch it off for a while, and back on | `claude-session-sync pause` / `resume` |
| Settle a conflict that `status` reports | `claude-session-sync resolve <id>` (or `resolve --all`) |
| Get back a session that was removed everywhere | `claude-session-sync restore <id>` |
| Take back what it did since some time | `claude-session-sync undo --since "2026-01-31 14:00"` |
| Change its settings | `claude-session-sync install --delete-sync` (run install again with the options you want) |
| Update it | run the install line again |
| Remove it | `claude-session-sync uninstall` (add `--purge` to delete its files too) |

`<id>` is the session id that `status` prints, or its first six characters.
`claude-session-sync --help` has the full list, with the places things are in on your
system.

## How it works

The app stores one small record per session, in a folder per account:

| System | The app's session store |
|---|---|
| Windows | `%APPDATA%\Claude\claude-code-sessions\<account>\<organization>\local_<id>.json` |
| macOS | `~/Library/Application Support/Claude/claude-code-sessions/<account>/<organization>/local_<id>.json` |
| Linux | `~/.config/Claude/claude-code-sessions/<account>/<organization>/local_<id>.json` |

Transcripts live elsewhere (`~/.claude/projects`) and are not tied to an account. So a
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
  (`replaced`, 14 days, up to 200 MB).
- Before the first change of each day it zips the whole session store (`backups`; the
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
- Told to stop (by you, by a logout, by the system shutting down), the daemon finishes
  the change it is making and begins no other, also in the middle of a long pass.
- Only ordinary files are read. A pipe, a socket or a device in the store (or a link to
  one) is never opened, and is left out of the backup; nothing is read without a limit.
- On macOS and Linux everything it writes is yours alone to read: its own folder and
  the folders in it (mode 0700), its log, journal, backups and kept versions (0600),
  and the records it puts in the store (0600, as the app writes its own), whatever the
  umask. What an earlier version left readable by others is narrowed the next time the
  daemon starts or `install` runs. Only the launch agent and the systemd unit, which
  hold nothing private, are readable by all.
- It never uses the network.

Its own files are in `%USERPROFILE%\.claude-session-sync` (Windows) or
`~/.claude-session-sync` (macOS, Linux): `sync.log`, `journal.jsonl`, `backups`,
`replaced`, `quarantine`, and the program in `bin`.

## Undo and uninstall

```
claude-session-sync uninstall
```

removes the autostart (the scheduled task, the launch agent or the systemd unit) and the
`PATH` entry or link, and stops the daemon. The copies it made stay where they are (your
accounts keep the full list), and so do its backups and log.
`claude-session-sync uninstall --purge` also deletes the tool's own folder. (On Windows
the programs themselves go a few seconds later, since a running program cannot delete
itself there; if anything is left, it tells you what.)

To also take back what it did, quit the Claude app completely and run:

```
claude-session-sync undo --since "2026-01-31 14:00"
```

While the Claude app is running, `undo` stops and tells you to close it (the app could
write its own list back over what undo puts right); `--force` goes ahead anyway.

To go back to the state before you ever ran it: quit the app, empty the app's session
store (the folder in the table under [How it works](#how-it-works)), and unzip the
oldest file in `backups` there. Mind that this also takes sessions you started since
then off the list (their transcripts stay on disk); `undo` is the gentler way.

## Platforms

| System | State |
|---|---|
| Windows 10/11 (x64) | Supported. This is where the tool began and has run longest. |
| macOS (Apple silicon and Intel, one universal program) | Supported. |
| Linux (x64, arm64; a static program, systemd for the autostart) | **Experimental: not yet tested against the real app.** |

**Verified on:**

- Windows 11 with the Microsoft Store build of the app (5.1231): in daily use across
  several accounts.
- macOS 26 on Apple silicon with app 1.46388: the whole test suite, the daemon under
  launchd (install, restart after a crash, pause and resume, uninstall), and the real
  session store and app log are found and read correctly. Not yet watched on a Mac: an
  account switch in the app itself. The Intel half of the macOS program is built but
  has not been run on an Intel Mac.

What "experimental" means for Linux: the program builds, its whole test suite passes
there, and the daemon was run under a systemd user session against a made-up session
store. It has not been run against the Claude desktop app for Linux (a beta, for
Debian and Ubuntu). Where that app keeps its session store and its log
(`~/.config/Claude`, or under `$XDG_CONFIG_HOME`), and what its program is called, are
taken from how the app is built on the other systems, not from looking at it. If the
store is somewhere else, `preview` says it was not found and changes nothing; name the
folder in `CLAUDE_SESSION_SYNC_STORE`. Start with `claude-session-sync preview` and
please report what you find.

**Is the app running?** The records in the folder the app has loaded are not replaced
while it runs, so this is answered cautiously on every system: when in doubt, it is
running. It counts as running if one of your own processes is the app's program
(Windows: `claude.exe` from the app's install folder; macOS: a program called `Claude`
in the `Contents/MacOS` of any application bundle, so a renamed or second copy counts
too; Linux: `claude-desktop` or `claude` with the app's bundle beside it, or any
Electron program whose command line names an `app.asar` with "claude" in its path), or,
on macOS and Linux, if the lock the app's engine keeps in its data folder
(`SingletonLock`) names a process that is there, or if the app's log was written
moments ago (on Linux, where the rest is a guess: in the last 10 minutes). Another
user's app does not count: it has its own store.

Differences between the systems that you may notice:

- **Coming back after `stop`.** `stop` ends the daemon for the moment; what starts it
  brings it back: within 5 minutes on Windows and macOS, within a minute on Linux. To
  keep it off, use `pause`.
- **A daemon that hangs.** A daemon that has given no sign of life for 15 minutes is
  replaced on every system (until then `status` shows `STALLED`). On Windows a second
  copy, started every 5 minutes, ends it and takes its place. launchd and systemd never
  start a second copy beside a daemon that is still there, so on macOS and Linux the
  daemon watches itself: when its main loop has not come round for 15 minutes it ends
  itself (exit code 75, with a line in `sync.log`), and launchd or systemd starts a
  fresh one. Time the computer spends asleep does not count.
- **macOS: `pause`** disables the launch agent in launchd's own records, by name.
  `resume`, `install` and `uninstall` lift that again; launchd keeps listing the name as
  enabled afterwards, which has no effect.
- **`pause`, `uninstall` and `status` go by what launchd or systemd say**, not by which
  files are there. A daemon whose agent or unit file was deleted while it runs is still
  stopped and taken away properly, one that was disabled by hand while it runs is not
  taken for paused, and "stopped" is only said once launchd or systemd agree.

## Limits

- **Undocumented internals.** Tested against the Microsoft Store build of the desktop
  app (5.1231) on Windows; for the other systems see [Platforms](#platforms). If an
  update changes the record format or the log lines this tool reads, it becomes more
  cautious rather than more daring: records it does not understand are left alone, and
  if it cannot tell which account is loaded it stops replacing existing records. It may
  still stop being useful until it is updated.
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

Environment variables, for unusual setups:

| Variable | Meaning |
|---|---|
| `CLAUDE_SESSION_SYNC_HOME` | Where the tool keeps its own files. |
| `CLAUDE_SESSION_SYNC_STORE` | The app's `claude-code-sessions` folder, if it is not found. |
| `CLAUDE_CONFIG_DIR` | Honoured as Claude Code does: transcripts are in `<dir>/projects`. |

The daemon has to see them too. On Windows, set them as user variables (Windows
Settings). On macOS and Linux, set them in the shell you run `install` from: those that
are set at that moment are written into the launch agent or the systemd unit, and
`install` says so. Commands you run later (`status`, `undo`, ...) look in the same
places only if the variables are set then as well. On macOS and Linux the program
itself is put in the `bin` folder of `CLAUDE_SESSION_SYNC_HOME`.

Linux: `XDG_CONFIG_HOME`, if it is set, says where the app's folder is looked for, and
is handed on to the daemon in the same way. It does not decide where the systemd unit
goes: that is the folder the running systemd reads user units from, which `install`
asks it for.

## Build from source

```
git clone https://github.com/ssgardaf/claude-session-sync
cd claude-session-sync
cargo test --release
cargo build --release
```

Rust 1.88 or later, on Windows, macOS or Linux. Every dependency is pure Rust (on
Windows both the MSVC and the GNU toolchain work). The result is in `target/release`:

- Windows: `claude-session-sync.exe` is the command-line tool, and
  `claude-session-syncw.exe` the same program without a console window, which the
  scheduled task runs.
- macOS and Linux: `claude-session-sync` is all there is. (A `claude-session-syncw` is
  built there as well; it is the same program, and is not installed or released.)

The test suite (more than 200 tests) works in throwaway folders and never writes to your
real profile. A few tests read the process list; on Windows a few also read the task
list. The tests of `install`, `uninstall`, `pause` and `resume` never reach Task
Scheduler, launchd or systemd: they record what would be run.

## License

MIT. See [LICENSE](LICENSE).
