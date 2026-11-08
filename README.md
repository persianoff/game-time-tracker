# GameTime

A tiny background tracker for Windows that records how long you play games. It only collects data. A separate single-file HTML viewer shows the calendar and stats on any machine.

- Detects games installed by Steam, Epic, GOG, Ubisoft, EA and Xbox, plus any you add by hand.
- Counts only time when the game window is in the foreground. Alt-tabbing out for up to 60 s (`merge_gap_s`, inclusive) keeps the same session.
- Native Rust, two small exes, no installer, no runtime. Target: about 1-2 MB private bytes (working set under 4 MB) and near-zero CPU.
- Data is stored as plain files in `%APPDATA%\GameTime\` and can be exported as CSV, JSON or ICS.

## Build from source

1. Install Rust: `winget install Rustlang.Rustup`. No MSVC Build Tools are needed.
2. In the repo, run `cargo build --release`. `rust-toolchain.toml` pins `stable-x86_64-pc-windows-gnu`, and rustup installs it on first use.
3. The result is in `target\release\`:
   - `gametime.exe`: the command-line tool.
   - `gametime-tracker.exe`: the background tracker (no console window).

`scripts\package.ps1` builds release and zips both exes, this README, `docs\CHECKLIST.md` and the viewer (`viewer\index.html`, `viewer\sample-sessions.json`) into `dist\gametime-<version>.zip`. Inside the zip the viewer files sit in a `viewer\` folder.

## Install

There is no installer. Copy **both exes into one folder** that you will not move later, for example `C:\Tools\GameTime\`. `.\gametime.exe autostart on` looks for `gametime-tracker.exe` next to `gametime.exe`. The examples below are run from that folder, so they use `.\gametime.exe`. To call `gametime` from anywhere, add the folder to your PATH (for example `[Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','User') + ';C:\Tools\GameTime', 'User')`), then open a new console.

## First run

```
.\gametime.exe scan             # check that your games are detected
.\gametime.exe autostart on     # register for logon AND start the tracker now
.\gametime.exe status
```

`autostart on` writes the per-user Run value `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\GameTime`. It needs no admin rights. The entry is visible in **Task Manager -> Startup apps**, where you can disable it. The tracker respects that choice and does not re-enable it. Running `.\gametime.exe autostart on` again re-registers the Run value and sets `autostart: true`, but it does not undo a Task Manager disable; it prints a warning instead.

Self-heal: at every start, `gametime-tracker.exe` (never `gametime run`) rewrites the Run value to its own path if it differs. It does so only when `autostart` is `true` in the config, the config could be read, and the Startup entry is not disabled in Task Manager. To move the exes, stop the tracker, move the folder, and launch `gametime-tracker.exe` once from the new place.

## Commands

```
.\gametime.exe scan                      List detected games and their paths
.\gametime.exe status                    Current session, today's totals, last 7 days
.\gametime.exe export --format csv|json|ics [--since YYYY-MM-DD] [--out PATH|-]
                                         --since keeps sessions whose local start date is
                                         on or after the date; --out - writes to stdout;
                                         default file: gametime-sessions.<ext> in the current folder
.\gametime.exe autostart on|off|status   on: register Run entry and start the tracker;
                                         off: remove the entry (tracker keeps running)
.\gametime.exe stop                      Close the open session and stop the tracker cleanly
.\gametime.exe run [--verbose]           Run the tracker in the foreground with console logging
.\gametime.exe --help                    Show help
```

## Configuration

`%APPDATA%\GameTime\config.json` is created with defaults on first use. A partial file is fine: missing keys take their defaults. A malformed file is left untouched: `scan` and `autostart` refuse to run and print the parse error, while the tracker falls back to defaults, logs `config error (...); using defaults` in `log.txt` and does not touch the Run entry. Restart the tracker (`.\gametime.exe stop`, then start it again) after editing.

| Key | Default | Meaning |
|---|---|---|
| `merge_gap_s` | `60` | Alt-tab away for up to this many seconds (inclusive) and the session resumes; longer closes it. |
| `autostart` | `true` | Whether the tracker registers/repairs its Run entry. `.\gametime.exe autostart on/off` sets it. |
| `export_dir` | `null` | Absolute folder (for example a OneDrive or network share). When set, the tracker rewrites `sessions.json`, `daily.csv`, `daily.json` and `gametime.ics` there. A relative path is ignored. |
| `export_interval_min` | `15` | Minutes between auto-exports. |
| `http_port` | `null` | When set (1-65535), serves the viewer and data over HTTP. See the security note below. `0` is rejected. |
| `extra_games` | `{}` | Map of exe or folder path to display name, for games detection misses, for example `{"D:\\Games\\Foo\\foo.exe": "Foo"}`. |
| `ignore` | `[]` | Paths (or exe names) to never track. `"D:"` means the whole drive. |

## Data

Everything lives in `%APPDATA%\GameTime\`:

| File | Content |
|---|---|
| `sessions.jsonl` | Append-only log, one line per closed session: `{game, source, exe, start_utc, end_utc, active_s}`. |
| `current.json` | Checkpoint of the open session (rewritten about every 30 s). Recovered at next start after a crash or power loss. |
| `current.<ts>.pending.json` | An unreadable checkpoint set aside for retry. |
| `config.json` | Settings (above). |
| `log.txt` | Small log, rotated at 1 MB. |

Exports and the HTTP endpoint share this contract. `sessions.json`:

```json
{ "version": 1, "generated": "...",
  "sessions": [ { "game": "...", "source": "...", "start": "2026-11-08T20:00:00+01:00",
                  "end": "2026-11-08T21:30:00+01:00", "active_s": 5400, "open": true } ] }
```

`start` and `end` are ISO-8601 with the local offset. `open: true` marks the in-progress session (auto-export and HTTP only). `daily.json` is `{version, generated, days:[{date, game, minutes}]}`; `daily.csv` has the columns `date,game,minutes`. `gametime.ics` has one event per session and can be imported into Google or Outlook Calendar.

## Viewer

`viewer\index.html` (in the zip: `viewer\`) is one self-contained file (vanilla JS, no libraries). It needs Chrome/Edge 123+, Firefox 120+ or Safari 17.5+ for dark mode; older browsers get a light-only fallback. Three ways to feed it:

1. **Served by the tracker.** Set `http_port`, then open `http://<game-pc>:<port>/` from any machine. Data loads live. The page is embedded in the tracker, so no file is needed.
2. **`?src=` parameter.** Open `index.html?src=http://<game-pc>:<port>/sessions.json` (the page may be a local file).
3. **File.** Open `index.html` and pick or drag in a `sessions.json` (for example from `export_dir`). `viewer\sample-sessions.json` is a demo data set.

The last source is remembered in the browser.

### HTTP security note

With `http_port` set, the tracker listens on **0.0.0.0** with **no authentication** and sends `Access-Control-Allow-Origin: *`. Anyone who can reach the port can read your play history. Use it on a **trusted LAN only**, and never expose it to the internet. The firewall rule below also limits access to the local subnet.

Routes (GET and HEAD only; other methods get 405, unknown paths 404): `/` (the viewer), `/sessions.json`, `/daily.json`, `/gametime.ics`. `daily.csv` is written only by auto-export (`export_dir`) and is not served.

Windows Firewall blocks inbound connections by default, and the app never changes firewall rules. Add one yourself, in an **administrator** PowerShell, limited to the Private profile and the local subnet (replace 8765 with your port):

```powershell
New-NetFirewallRule -DisplayName "GameTime viewer" -Direction Inbound -Protocol TCP -LocalPort 8765 -Action Allow -Profile Private -RemoteAddress LocalSubnet
```

Remove it later with `Remove-NetFirewallRule -DisplayName "GameTime viewer"`. Make sure your home network is set to the Private profile.

## Troubleshooting

- **A game is never counted, or `log.txt` says access denied.** Some anti-cheat systems deny access to the game process, so its exe path cannot be read. The tracker logs this once and keeps running. If the game is detected but not counted, there is no workaround; that game cannot be tracked.
- **A game is not listed by `.\gametime.exe scan`.** Add it to `extra_games` (exe or install folder -> display name) and restart the tracker.
- **Something looks wrong.** Read `%APPDATA%\GameTime\log.txt`, or stop the background tracker (`.\gametime.exe stop`) and run `.\gametime.exe run --verbose` to watch events live.
- **Logoff or shutdown loses up to 30 s.** This is an accepted limitation: the open session is recovered from the checkpoint at the next start.

## Uninstall

```
.\gametime.exe autostart off
.\gametime.exe stop
```

Then delete the exe folder and, if you want to drop the data, `%APPDATA%\GameTime\`. Remove the firewall rule if you added one.

## License

MIT. See [LICENSE](LICENSE).
