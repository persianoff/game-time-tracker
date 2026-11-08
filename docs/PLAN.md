# GameTime: a Windows game-time tracker in Rust (collector only)

## Context
We want a tiny background app for Windows that records how long games are played, so the time can later be shown on a calendar. The tracker only collects. A separate, zero-install HTML viewer shows the calendar and stats on any machine.

Decisions from the user:
- **Rust**, for the smallest RAM footprint. Target: private bytes about 1–2 MB, working set under 4 MB (see the decisions log), near-zero CPU, and one native `.exe` with no runtime needed.
- **Minimal dependencies.**
- **Game detection:** auto-detect games installed by launchers (Steam, Epic, GOG, Ubisoft, EA, Xbox).
- **Time rule:** count only time when the game window is in the foreground.
- **Output:** stats only, exported in portable formats.

## Decisions log
Decisions made during implementation. Where they differ from the original design below, these win.

| Topic | Decision | Source |
|---|---|---|
| Target machine | The tracker runs on a **separate gaming PC**. The dev machine has no launchers, so detection is tested with fixtures there, and the end-to-end check (#14) runs on the gaming PC. | User, 2026-11-08 |
| Toolchain | `stable-x86_64-pc-windows-gnu`, pinned in `rust-toolchain.toml`. No MSVC Build Tools. | User, 2026-11-07 |
| `windows-sys` | Pinned to **0.59**: it ships prebuilt GNU import libs, while 0.61+ needs `dlltool` and `as`. | #18 |
| Memory target | Measured as **private bytes ~1–2 MB, working set < 4 MB after trim**. The tracker calls `SetProcessWorkingSetSize(-1,-1)` after startup and after each rescan. Measured: 0.5–0.6 MB working set, 1.2–1.3 MB private. | #23 |
| Logoff / shutdown | No hidden window or `WM_ENDSESSION` handling. Losing up to ~30 s is accepted, and the session is recovered from the checkpoint at the next start. | User, 2026-11-08 |
| Time attribution | Sample-and-hold: each poll interval is credited to the game that was in the foreground at the previous poll, when the gap is ≤ 10 s. | #17 |
| Session state machine | It lives in `session.rs`, and `tracker.rs` holds the engine and the loop. `step()` returns `Option<Session>` rather than events. | #17 |
| Checkpoints | Written only while the slot is free and owned. `recover()` returns `Recovery { recovered, checkpoint_free, errors }`, and an unreadable checkpoint is set aside as `current.<ts>.pending.json`. | #20, #23 |
| Rescan | Every 30 min, but only while no session is open or paused. | #23 |
| Detection depth | Install dirs one level below a drive root are accepted (for example `D:\Fortnite`). Bare library roots (`D:\Games`, `D:\SteamLibrary` and similar) are rejected for auto sources but allowed in `extra_games`. | #21 |
| Viewer days | Days are computed in the **gaming PC's offset**, which is embedded in each ISO time, never in the browser's time zone. A session crossing midnight is split by wall-clock overlap. | #19 |
| Viewer colors | Colors are assigned by **all-time rank**: the top 8 games get distinct slots and the rest share "other". Every text/fill pair must reach ≥ 4.5:1 contrast in both themes, enforced by a self-test. | #28 |
| Detection (auto sources) | Auto sources that point at a launcher's own default folder are rejected like library roots, for example `Program Files\Epic Games` or `steamapps\common`. Manual `extra_games` entries are still allowed. An `ignore` value of `D:` means the whole drive. | #32 |
| Viewer browsers | The viewer uses CSS `light-dark()`, so it needs Chrome/Edge 123+, Firefox 120+ or Safari 17.5+ Older browsers get a light-only fallback via `@supports not`, with a note when dark mode is requested. | #28, #31 |
| Daily aggregation | Rust: `stats::split_by_local_day` with a per-instant offset (DST-aware). Sessions are capped at 7 days, and times beyond year 9999 are ignored. | #25 |
| Autostart self-heal | Only `gametime-tracker.exe` repairs the Run entry, never `gametime run`, and never when the config failed to load or Task Manager has the entry disabled. | #27 |
| Auto-export | Payloads are built on the poll thread and written on a short-lived thread, at most one in flight, without fsync. A relative `export_dir` is ignored. `daily.*` moved to #11. | #26 |

## Step 0: toolchain
- Install `winget install Rustlang.Rustup`, then `rustup default stable-x86_64-pc-windows-gnu` (see the decisions log).
- Check with `cargo --version`.

## Dependencies (kept minimal)
| Crate | Why |
|---|---|
| `windows-sys` (Microsoft) | Thin, zero-cost Win32 bindings: foreground window, process path, registry, mutex, time-zone conversion. |
| `serde_json` (+ `serde` derive) | Parse Epic `.item` manifests, read and write config and JSON exports. |

Everything else is hand-written with `std`, because it is small:
- **VDF/ACF parser.** Steam's key-value text format, about 60 lines.
- **CSV and ICS writers.** Plain string formatting.
- **HTTP endpoint.** `std::net::TcpListener`, GET-only, a few static routes.
- **Time.** Store UTC epoch seconds; convert to local time with Win32 `SystemTimeToTzSpecificLocalTime`. No `chrono` needed.
- **Storage.** Append-only files instead of SQLite (see below). No C compiler needed.

Release profile: `opt-level="z"`, `lto=true`, `codegen-units=1`, `panic="abort"`, `strip=true`. Expected exe size: about 300–600 KB.

## Layout
```
TT/
  Cargo.toml
  src/
    main.rs        CLI entry (gametime.exe): scan | status | export | install | uninstall | run
    tracker_bin.rs second bin gametime-tracker.exe with #![windows_subsystem="windows"] (no console window)
    autostart.rs   Run-key register/verify/self-heal, StartupApproved check, stop event
    win32.rs       foreground exe path, single-instance mutex, registry helpers, local-time conversion
    detect.rs      launcher scanning → Vec<(install_prefix, game_name, source)>
    vdf.rs         Steam VDF/ACF parser
    tracker.rs     polling loop + session state machine (pure logic separated for tests)
    store.rs       append-only session log + checkpoint
    export.rs      CSV / JSON / ICS writers, optional HTTP endpoint
    config.rs      %APPDATA%\GameTime\config.json
  viewer/
    index.html            standalone calendar/stats viewer (also embedded in tracker)
    sample-sessions.json  test fixture
  README.md
```
Two binaries come from one crate. The tracker runs with no console window. The CLI prints to the terminal.

## Components

### win32.rs
- Foreground lookup: `GetForegroundWindow` → `GetWindowThreadProcessId` → `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` → `QueryFullProcessImageNameW`, giving the full exe path.
- If the process can't be opened (some anti-cheat games block it): return `None`, log it once, keep running.
- `CreateMutexW("Local\\GameTime")` so only one tracker runs.
- Registry reads (`RegGetValueW`, `RegEnumKeyExW`) for launcher discovery and the `HKCU\...\Run` autostart value.

### detect.rs: map install folders to game names
- **Steam:** `HKCU\Software\Valve\Steam\SteamPath` → `steamapps\libraryfolders.vdf` → each library's `appmanifest_*.acf`, using `name` and `installdir`.
- **Epic:** `C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests\*.item`, using `DisplayName` and `InstallLocation`.
- **GOG:** `HKLM\SOFTWARE\WOW6432Node\GOG.com\Games\*`, using `gameName` and `path`.
- **Ubisoft:** `HKLM\SOFTWARE\WOW6432Node\Ubisoft\Launcher\Installs\*\InstallDir`, named after the folder.
- **EA:** `HKLM\SOFTWARE\WOW6432Node\Electronic Arts\*`, using `Install Dir`. Also the HKLM `Uninstall` keys (both 64-bit and `WOW6432Node`) whose `Publisher` contains "Electronic Arts", using `DisplayName` and `InstallLocation`. A denylist skips the EA app, Origin, anti-cheat, redistributable, background-service and helper entries.
- **Xbox:** `<drive>:\XboxGames\*`, plus the folders listed in `<drive>:\.GamingRoot`. Both header layouts are accepted, and if none parses, the first 32 bytes are logged once in hex.
  - Rescans probe only the drives already known to have Xbox games. A full sweep of all fixed drives runs on the first scan and every 6 h.
- **Exclusions:** launcher and helper exes (`steam.exe`, `steamwebhelper.exe`, `EpicGamesLauncher.exe`, `UnityCrashHandler*`, `CrashReport*`, `vc_redist*`, `dxsetup*`).
- **Overrides:** `config.json` has `extra_games` (path → name) and `ignore` (paths).
- **Matching:** case-insensitive longest-prefix match on the exe path.
- **Refresh:** re-scan at start and every 30 min. Cache the last foreground pid → result, so a match isn't recomputed every second.

### tracker.rs: session state machine
- Poll every **1 s** (`std::thread::sleep`), then resolve foreground → game or none.
- A session starts when a game gets focus.
- When focus leaves, the session pauses. If the same game regains focus within **`merge_gap_s` (default 60)**, the session resumes. Otherwise it closes.
- Each session records `start`, `end` and `active_seconds`, the summed foreground time.
- **Sleep, hibernate or clock jump:** a wall-clock gap over 10 s between polls closes the session at the last poll. A lock screen (`LockApp.exe`) counts as non-game.
- The pure state machine (`fn step(state, now, fg_game) -> events`) is kept free of Win32 calls, so it is unit-testable.

### store.rs: append-only files (no database)
- Files go in `%APPDATA%\GameTime\`:
  - `sessions.jsonl`: one JSON line per closed session, `{game, source, exe, start_utc, end_utc, active_s}`.
  - `current.json`: checkpoint of the open session, rewritten every 30 s.
  - `log.txt`: a small rotating log.
- On startup, a leftover `current.json` (from a crash or power loss) is closed and appended at its last checkpoint. At most 30 s is lost.
- Why files: zero extra dependencies, readable by anything, and easy to sync. At a few KB per month, reading the whole file for `status` and `export` is instant.

### export.rs: data for the external viewer
- `gametime export --format csv|json|ics [--since YYYY-MM-DD] [--out PATH]`:
  - **Sessions:** one row each (game, source, local start and end, minutes).
  - **Daily aggregate:** `date, game, minutes`.
  - **ICS:** one VEVENT per session, importable into Google or Outlook Calendar.
- **Auto-export (optional):** when `config.export_dir` is set (for example a OneDrive or network share), the tracker rewrites `sessions.json`, `daily.csv` and `gametime.ics` there every `export_interval_min` (default 15).
- **HTTP endpoint (optional):** with `config.http_port` set, a std `TcpListener` thread serves `/sessions.json`, `/daily.json` and `/gametime.ics` on the LAN. Off by default.

### Autostart (autostart.rs)
- **Mechanism:** the per-user Run key `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `GameTime` = `"<full path>\gametime-tracker.exe"`. It starts at every logon, needs no admin rights, and shows up in Task Manager's Startup tab, so the user can see and control it.
- **On by default:** on first launch, the tracker registers itself when `config.autostart` is `true`, which is the default.
- **Self-healing:** at every start, the tracker checks the registered path against its own `current_exe()` and rewrites the value if the exe was moved or updated.
- **Respects the user's choice:** if the user disables it in Task Manager (`...\Explorer\StartupApproved\Run` shows disabled), the tracker leaves it disabled and doesn't re-enable it.
- **Runs straight after install:** `gametime autostart on` also launches the tracker immediately, if it isn't already running (thanks to the mutex), so tracking starts without a logoff.
- **Recovery:** with `panic="abort"` a crash would end tracking until the next logon. To limit that, the main loop catches and logs all recoverable errors instead of panicking. If more robustness is needed later, swap the Run key for a Task Scheduler task with "restart on failure". Not in v1.
- **Uninstall:** `gametime autostart off` removes the value. `gametime stop` signals the running tracker to close its session and exit cleanly, through a named event (`Local\GameTimeStop`).

### CLI (gametime.exe)
- `scan`: list the detected games and their paths.
- `status`: show the current session, today's totals and the last 7 days.
- `export ...`: write the files described under export.rs.
- `autostart on|off|status`: manage the Run-key entry (see Autostart). `on` also starts the tracker right away.
- `stop`: cleanly stop the running tracker.
- `run --verbose`: run the tracker in the foreground with console logging, for debugging.

## External viewer: `viewer/index.html`
The viewer is one self-contained HTML file with vanilla JS and CSS: no libraries, no build step, no install. It runs in any browser on any machine (Windows, Mac, Linux, phone).

### Getting data into the viewer
Three ways, in order of convenience:
1. **Served by the tracker.** When `http_port` is set, the tracker embeds `index.html` with `include_str!` and serves it at `/`. The other machine opens `http://<game-pc>:<port>/` and data loads live from `/sessions.json`.
2. **URL parameter.** `index.html?src=http://<game-pc>:<port>/sessions.json`. The tracker sends `Access-Control-Allow-Origin: *`, so the page can also be opened from a file.
3. **File.** Open the page locally and use the file picker or drag-and-drop on a `sessions.json` (for example from the synced `export_dir`).

The last-used source is remembered in `localStorage`.

### Data contract (shared by tracker and viewer)
`sessions.json` looks like `{ "version": 1, "generated": "...", "sessions": [ {game, source, start, end, active_s, open?} ] }`. `start` and `end` are ISO-8601 with the local offset. The viewer splits sessions that cross midnight across the two days.
- `"open": true` is optional. It marks the in-progress session, and only the tracker's auto-export emits it. CSV, ICS and the CLI export contain closed sessions only.
- Deduplication: records with the same `game` and `start` are collapsed. The closed record wins over an open one; otherwise the later `end` wins. This applies in both the store (`load_all`) and the viewer.

### Views
- **Month calendar (main view).** A 7-column grid with one cell per day:
  - total play time, with cell shading by minutes (a sequential scale);
  - small stacked bars for the top games that day, using a stable color per game;
  - clicking a day opens a **day timeline**: a 24-hour bar showing each session block, with hover details.
- **Year heatmap.** A GitHub-style grid with one square per day, for spotting streaks and breaks.
- **Per-game totals** for the selected range (week, month, year, all). A sorted horizontal bar list with session count and average session length. Clicking a game filters all views to it.
- **Summary tiles.** Total hours in the range, number of days played, longest session, and most-played game.
- **Navigation.** Previous and next month or year, a game filter, light and dark themes (following `prefers-color-scheme`).

### Implementation notes
- Charts are inline SVG generated by small JS functions (`renderMonth`, `renderHeatmap`, `renderTotals`, `renderDay`).
- Colors and marks follow the `dataviz` skill, which gets loaded before writing chart code: a categorical palette for games and a sequential scale for intensity.
- Aggregation (`byDay`, `byGame`, midnight splitting) lives in a pure-JS section that can be checked in the browser console against known data.
- A `viewer/sample-sessions.json` fixture with generated data supports development and testing.

## Verification
1. `cargo test`. Tests cover the VDF and ACF parser, Epic manifest parsing, prefix matching with exclusions, the state machine (alt-tab within and beyond the merge gap, a sleep gap, switching between two games), crash-checkpoint recovery, and ICS and CSV formatting.
2. `cargo build --release`, then `gametime scan` should list the installed games correctly.
3. `gametime run --verbose`. Play a game, alt-tab out for under and over 60 s, then quit. Check the start, pause, resume and close events in the log and that the times match a stopwatch.
4. `gametime status` should show the expected totals.
5. `gametime export --format ics` → import the file into a calendar and check the events. Also check the CSV and JSON files.
6. **Autostart:**
   - `gametime autostart on`: the tracker starts immediately, and the entry appears in Task Manager → Startup.
   - Log off and on: `gametime-tracker.exe` is running, with a working set of about 2–4 MB.
   - Move the exe and launch it once: the Run value is updated to the new path.
   - Disable the entry in Task Manager and restart the tracker: it stays disabled.
   - `gametime stop` → the open session is saved and the process exits. Then `autostart off` → the entry is gone.
7. **Viewer:**
   - Open `viewer/index.html` with `sample-sessions.json`. Check the month, heatmap, totals and day-timeline views, a session that crosses midnight, and dark mode.
   - Turn on `http_port`, open `http://<game-pc>:<port>/` from another machine, and confirm the live data matches `gametime status`.
