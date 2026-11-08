# Gaming-PC verification checklist (issue #14)

Run this on the gaming PC with the real launchers installed. Derived from PLAN section Verification. Tick each box and paste the results listed at the end into issue #14.

Setup: unzip the package into a permanent folder, for example `C:\Tools\GameTime\` (it contains both exes, `README.md`, this file and `viewer\`). Open PowerShell there and use `.\gametime.exe` below.

## 1. Detection

- [ ] `.\gametime.exe scan` runs without errors.
- [ ] Per launcher, every installed game you expect is listed with the right name and path:
  - [ ] Steam (all library folders)
  - [ ] Epic
  - [ ] GOG
  - [ ] Ubisoft
  - [ ] EA (only real games)
  - [ ] Xbox / Game Pass
- [ ] EA sanity: no EA app, Origin, anti-cheat, redistributable or helper entries appear in the list.
- [ ] No bare library folders (`D:\Games`, `D:\SteamLibrary`, `Program Files\Epic Games`) appear as games.
- [ ] Missing games are noted (add them via `extra_games`, restart the tracker, and check that they then appear).
- [ ] Xbox `.GamingRoot` dump taken for each drive that has one. Capture the **full** dump, not just the first lines, so it can become a test fixture:

  ```powershell
  Format-Hex D:\.GamingRoot
  ```

  Adjust the drive letter. If `log.txt` contains a line about an unparsed `.GamingRoot` (first 32 bytes in hex), paste that too.

## 2. Session timing (stopwatch)

First stop any running tracker: `.\gametime.exe stop`. Then run `.\gametime.exe run --verbose` in a console and keep it open. The log has two kinds of session lines:

- `tracking <game> (<source>)` when a session starts (also printed again when you return after a close).
- `session closed: <game> <N>s active` when a session closes. There are no pause or resume lines.

Test:

- [ ] Start a game and focus it. A `tracking <game> (...)` line appears. Start the stopwatch.
- [ ] Alt-tab out for **up to 60 s**, then back. **No** new `tracking` line and no `session closed` line appears; it is still one session.
- [ ] Alt-tab out for **over 60 s**. About 61 s after you left, `session closed: <game> <N>s active` is printed (while you are still away). `N` is the time in the game before you left. When you return, a new `tracking <game> (...)` line appears.
- [ ] Quit the game. About 60 s later (the merge gap) `session closed: <game> <N>s active` is printed for the last session.
- [ ] The `N` values match the stopwatch within a few seconds. Time spent alt-tabbed out is not counted, so for the under-60 s test the final session's `N` equals the total play time minus the alt-tab time.
- [ ] In a second console, run `.\gametime.exe stop`. The verbose tracker prints `tracker stopped` and exits. If a game was open at that moment, its session is saved (check with `status`). (Ctrl+C is not a clean stop: it kills the process, and the open session is recovered from the checkpoint at the next start, losing up to 30 s.)

## 3. Status and exports

- [ ] `.\gametime.exe status` shows today's total and the last 7 days, matching what you just played.
- [ ] `.\gametime.exe export --format csv --out $env:TEMP\gt.csv` opens correctly in Excel.
- [ ] `.\gametime.exe export --format json --out $env:TEMP\gt.json` has the expected sessions.
- [ ] `.\gametime.exe export --format ics --out $env:TEMP\gt.ics`, import into Google or Outlook Calendar: the events have the right game, start and end.

## 4. Autostart

- [ ] `.\gametime.exe autostart on`: the tracker starts at once (`Get-Process gametime-tracker`), and **Task Manager -> Startup apps** lists GameTime as Enabled.
- [ ] Registry check (first byte of the StartupApproved value: even = enabled, odd = disabled; a missing value counts as enabled):

  ```powershell
  (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run').GameTime
  $v = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run' -ErrorAction SilentlyContinue).GameTime; if ($v) { $v[0] } else { 'missing (= enabled)' }
  ```

  The first prints the quoted path to `gametime-tracker.exe`.
- [ ] Log off and log on: `gametime-tracker` is running without any manual action.
- [ ] Working set after logon, target 2-4 MB (or lower), and CPU near 0 in Task Manager:

  ```powershell
  Get-Process gametime-tracker | Select-Object Name, WS, PM, @{n='WS_MB';e={[math]::Round($_.WorkingSet64/1MB,2)}}, @{n='PM_MB';e={[math]::Round($_.PagedMemorySize64/1MB,2)}}
  ```

- [ ] Move the exe: `.\gametime.exe stop`, move the whole folder (for example to `C:\Tools\GameTime2`), `cd` into it, run `.\gametime-tracker.exe` once, then `.\gametime.exe autostart status`. It must show `matches: true` and the Run value must point to the new path. Repeat the same sequence to move it back (stop, move, cd, `.\gametime-tracker.exe`, `autostart status`).
- [ ] Disabled state: in Task Manager -> Startup apps, **Disable** GameTime (the StartupApproved first byte is now odd). Run `.\gametime.exe stop`, then start the tracker with `.\gametime-tracker.exe`. It must stay Disabled in Task Manager and `autostart status` should show it as disabled.
- [ ] Re-enable it in Task Manager. Start a game, alt-tab to the console and within 60 s run `.\gametime.exe stop`. The session is saved, see `log.txt` / `status`.
- [ ] `.\gametime.exe autostart off`: the Run value is gone (`autostart status` shows `registered: (none)`) and the entry disappears from Task Manager.
- [ ] Optional: confirm that up to 30 s lost at logoff or shutdown mid-game is recovered at next start (accepted limitation).

## 5. Viewer

- [ ] Open `viewer\index.html` locally and load `viewer\sample-sessions.json`: month, heatmap, totals, day timeline, a session crossing midnight, and dark mode all look right.
- [ ] Set `"http_port": 8765` in `%APPDATA%\GameTime\config.json`, then `.\gametime.exe stop` and `.\gametime.exe autostart on` (or start `.\gametime-tracker.exe`).
- [ ] Check `%APPDATA%\GameTime\log.txt` contains `http endpoint listening on 0.0.0.0:8765`. If it says `http endpoint disabled (port 8765): ...`, the port is taken. If it says `config error (...); using defaults`, fix the JSON syntax. As a syntax check, `.\gametime.exe scan` prints `cannot read config: ...` when the JSON is malformed.
- [ ] Add the firewall rule (administrator PowerShell, Private profile, local subnet only):

  ```powershell
  New-NetFirewallRule -DisplayName "GameTime viewer" -Direction Inbound -Protocol TCP -LocalPort 8765 -Action Allow -Profile Private -RemoteAddress LocalSubnet
  ```

- [ ] From **another machine** open `http://<game-pc>:8765/`: the viewer loads and the live data matches `.\gametime.exe status`, including the in-progress session while a game is running.
- [ ] `http://<game-pc>:8765/sessions.json`, `/daily.json` and `/gametime.ics` all respond.
- [ ] Optionally open `viewer\index.html?src=http://<game-pc>:8765/sessions.json` from a local file on the other machine.
- [ ] Afterwards, remove the firewall rule if you do not want it (`Remove-NetFirewallRule -DisplayName "GameTime viewer"`) and set `http_port` back to `null`.

## Report back

Paste these into issue #14:

1. Windows version and which launchers are installed.
2. Output of `.\gametime.exe scan`, plus a list of games that are missing or wrongly included, and any EA oddities.
3. The full `Format-Hex <drive>:\.GamingRoot` output for each drive that has the file (or "none"), and any `.GamingRoot` line from `log.txt`.
4. Output of `.\gametime.exe status` after your test session.
5. Stopwatch results: expected vs. logged `N` seconds for the under-60 s and over-60 s alt-tab tests, with the matching `tracking` / `session closed` log lines.
6. The working-set and private-bytes numbers from the PowerShell command above after logon, and idle CPU.
7. Output of the two registry commands (Run value and StartupApproved first byte) in the enabled and disabled states.
8. Pass or fail for each remaining box (ICS import, autostart logoff, moved exe, Task Manager disabled state, `stop`, viewer from another machine), with any error text copied verbatim.
9. The last 50 lines of `%APPDATA%\GameTime\log.txt` if anything failed.
