# Gaming-PC verification checklist (issue #14)

Run this on the gaming PC with the real launchers installed. Derived from PLAN section Verification. Tick each box and paste the results listed at the end into issue #14.

Setup: unzip the package into a permanent folder, for example `C:\Tools\GameTime\`, and open PowerShell there. Use `.\gametime.exe` below.

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
- [ ] Missing games are noted (add them via `extra_games` and re-check that they then appear).
- [ ] Xbox `.GamingRoot` hex dump taken for each drive that has one (`Format-Hex D:\.GamingRoot | Select -First 4`, adjust the drive letter). Paste it in the report. If `log.txt` mentions an unparsed `.GamingRoot`, paste that line too.

## 2. Session timing (stopwatch)

Stop any running tracker first (`.\gametime.exe stop`), then run `.\gametime.exe run --verbose` in a console and keep it open.

- [ ] Start a game and focus it. Log shows a session start. Start the stopwatch.
- [ ] Alt-tab out for **under 60 s**, then back. Log shows pause then resume, with no new session.
- [ ] Alt-tab out for **over 60 s**, then back. Log shows the session closed, then a new one started.
- [ ] Quit the game. Log shows the close.
- [ ] Session lengths match the stopwatch within a few seconds (alt-tab time not counted).
- [ ] Ctrl+C the console. The open session is saved.

## 3. Status and exports

- [ ] `.\gametime.exe status` shows today's total and the last 7 days, matching what you just played.
- [ ] `.\gametime.exe export --format csv --out $env:TEMP\gt.csv` opens correctly in Excel.
- [ ] `.\gametime.exe export --format json --out $env:TEMP\gt.json` has the expected sessions.
- [ ] `.\gametime.exe export --format ics --out $env:TEMP\gt.ics`, import into Google or Outlook Calendar: the events have the right game, start and end.

## 4. Autostart

- [ ] `.\gametime.exe autostart on`: the tracker starts at once (`Get-Process gametime-tracker`), and **Task Manager -> Startup apps** lists GameTime as Enabled.
- [ ] Log off and log on: `gametime-tracker` is running without any manual action.
- [ ] Working set after logon, target 2-4 MB (or lower):

  ```powershell
  Get-Process gametime-tracker | Select-Object Name, @{n='WS_MB';e={[math]::Round($_.WorkingSet64/1MB,2)}}, @{n='PM_MB';e={[math]::Round($_.PagedMemorySize64/1MB,2)}}
  ```

  Also `Get-Process gametime-tracker | Select WS,PM` as raw bytes if you prefer. Check CPU stays near 0 in Task Manager.
- [ ] Move the folder (exe) elsewhere, run `gametime-tracker.exe` once from the new place, then `.\gametime.exe autostart status`: the Run value points to the new path. Move it back if you prefer.
- [ ] In Task Manager -> Startup apps, **Disable** GameTime, then `.\gametime.exe stop` and start `gametime-tracker.exe` by hand. It must stay Disabled (not re-enabled).
- [ ] Re-enable it in Task Manager, then `.\gametime.exe stop`: it prints `stopped`, the process exits, and a session open at that moment is saved (visible in `status`).
- [ ] `.\gametime.exe autostart off`: the entry is gone from Task Manager and `autostart status`.
- [ ] Optional: confirm up to 30 s lost at logoff/shutdown mid-game is recovered at next start (accepted limitation).

## 5. Viewer

- [ ] Open `viewer\index.html` locally with `sample-sessions.json`: month, heatmap, totals, day timeline, a session crossing midnight, and dark mode all look right.
- [ ] Set `"http_port": 8765` in `%APPDATA%\GameTime\config.json`, restart the tracker (`stop`, then `autostart on`).
- [ ] Add the firewall rule (administrator PowerShell, Private profile):

  ```powershell
  New-NetFirewallRule -DisplayName "GameTime viewer" -Direction Inbound -Protocol TCP -LocalPort 8765 -Action Allow -Profile Private
  ```

- [ ] From **another machine** open `http://<game-pc>:8765/`: the viewer loads and the live data matches `gametime status`, including the in-progress session while a game is running.
- [ ] `http://<game-pc>:8765/sessions.json`, `/daily.json` and `/gametime.ics` all respond.
- [ ] Optionally `index.html?src=http://<game-pc>:8765/sessions.json` opened from a local file works.
- [ ] Afterwards, remove the firewall rule if you do not want it (`Remove-NetFirewallRule -DisplayName "GameTime viewer"`) and set `http_port` back to `null`.

## Report back

Paste these into issue #14:

1. Windows version and which launchers are installed.
2. Output of `.\gametime.exe scan` (redact nothing needed), plus a list of games that are missing or wrongly included, and any EA oddities.
3. The `Format-Hex <drive>:\.GamingRoot | Select -First 4` output for each drive that has the file (or "none"), and any `.GamingRoot` line from `log.txt`.
4. Output of `.\gametime.exe status` after your test session.
5. Stopwatch results: expected vs. logged minutes for the under-60 s and over-60 s alt-tab tests.
6. The working-set / private-bytes numbers from the PowerShell command above after logon, and idle CPU.
7. Pass or fail for each remaining box (ICS import, autostart logoff, moved exe, Task Manager disabled state, `stop`, viewer from another machine), with any error text copied verbatim.
8. The last 50 lines of `%APPDATA%\GameTime\log.txt` if anything failed.
