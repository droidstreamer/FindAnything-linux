# Find Anything: Install, Check, Use and Uninstall

Instant filename search across every drive on Linux, like Everything on Windows. Two programs: `fa-gui` (the search window) and `fa` (the background service, or daemon, plus a command line). The daemon runs as root so it can watch every drive for changes.

## Requirements

- **System:** Linux with glibc 2.39+, GTK 4.12+ and libadwaita 1.5+ (Ubuntu 24.04+, Fedora 40+).
- **Kernel:** 5.17+ (6.8+ for btrfs).
- **Tools:** `pkexec` (preinstalled on Ubuntu and Fedora). The boot service (`install.sh`) needs systemd, also preinstalled on both.

## Install As A Service (Recommended, Ubuntu)

The daemon starts at every boot, before you log in, so the window opens ready: no Start button and no password. Since 0.22.0.

1. **Old Daemon:** stop the old SearchEverything daemon if you ever ran one (old window: menu > Stop Daemon).
2. **Copy:** put the build folder in your home folder:
   ```
   mkdir -p ~/Applications/FindAnything
   cp -r /path/to/FindAnything_v0.22.2 ~/Applications/FindAnything/
   ```
3. **Installer:** open a terminal in that folder and run it as yourself (not with sudo):
   ```
   cd ~/Applications/FindAnything/FindAnything_v0.22.2
   sh install.sh
   ```
   It lists what it will do and asks "Go ahead? [y/N]", then asks about the polkit rule. sudo asks for your password once. It does the following:
   - **Stops The Daemon:** a running daemon saves its index first.
   - **Copies Files:** `fa`, `fa-gui` and the notes go to `/opt/findanything` (owned by root); the previous `fa` and `fa-gui` stay as `fa.prev` and `fa-gui.prev`.
   - **Writes The Unit:** `/etc/systemd/system/findanything.service` runs `/opt/findanything/fa daemon` as root at boot. Its socket and log (`/opt/findanything/fa-<host>.log`) belong to you.
   - **Starts It:** `systemctl daemon-reload`, then enable and start the service, then it prints the first lines of `fa status`.
4. **Polkit Rule:** answering "y" lets your user Start, Stop and restart this one service from the window without a password (only from your own desk session, only `findanything.service`). The file is `/etc/polkit-1/rules.d/50-findanything.rules`. Answering "n" keeps the password prompt on Start.
5. **First Window:** start the window once from the installed copy so the app menu points there: `/opt/findanything/fa-gui`. From then on press Super, type "Find Anything", press Enter. To keep it in the dock, right-click its icon while it runs > Pin to Dash.
6. **First Index:** reading all drives takes a few minutes (the window shows progress; search waits until the drives are read). Every later start, including each boot, loads the saved index and only checks what changed while it was off.

### Check The Service

- **Status:** `systemctl status findanything` shows whether it runs and since when.
- **Journal:** `journalctl -b -u findanything` shows its start and stop messages for this boot.
- **Index:** `/opt/findanything/fa status` shows drives, items and how the last start went.

### Stop And Start

- **Stop:** status button (bottom right) > Stop. It stays stopped until you press Start or the computer restarts.
- **Boot Start:** to stop it starting at boot run `sudo systemctl disable findanything`; enable it again with `sudo systemctl enable findanything`.

### Update (A Newer Build)

- **Run The Installer:** copy the new build folder next to the old one, open a terminal in it and run `sh install.sh` again. It stops the service, copies the new files to `/opt/findanything` (keeping the old ones as `.prev`) and starts it. The saved index and settings carry over.
- **Reopen:** close the window and open it again from the app menu. If the window says the service is another version, the install was not run from that build's folder.

### Go Back To The Previous Version

After an update that misbehaves:

```
sudo systemctl stop findanything
sudo cp -p /opt/findanything/fa.prev /opt/findanything/fa
sudo cp -p /opt/findanything/fa-gui.prev /opt/findanything/fa-gui
sudo systemctl start findanything
```

### Drives Mounted Later

- **Late Mounts:** drives the desktop mounts after the service starts, and USB keys plugged in later, are picked up at once and read as their own drive. USB keys are dropped again when ejected. Since 0.22.1.
- **Full Details:** every file the service installs, where it lives, and a step-by-step clean uninstall are in `INSTALL_SERVICE.md` (in this folder).

## Without The Service (Start It By Hand Each Time)

- **Setup:** do steps 1 and 2 above, then start the window from the build folder: `~/Applications/FindAnything/FindAnything_v0.22.2/fa-gui`. This puts Find Anything in the app menu (the entry points at the `fa-gui` started last).
- **Start:** click Start Daemon (the Play button at the bottom right) and type your password. The daemon runs until you stop it or restart the computer; after a reboot, press Start again.
- **Update:** start the new build's `fa-gui` once, then Stop and Start the daemon (status button, bottom right) so it runs the new version. A bar under the search box offers Restart Daemon if the old one is still running.

## Use

- **Search:** type in the search box and results appear as you type. Enter searches at once.
- **Open:** double-click (or Enter) opens a file in its default app; Ctrl+Enter opens its folder; right-click for more.
- **Limit:** the window shows the first 100,000 results; the count in the status bar is always exact. Type more to narrow it.

### Search Words

- **Both Words:** `holiday photo` matches both words, any order.
- **Either Word:** `jpg | png` matches either word.
- **Leave Out:** `!thumb !BACKUP` leaves out anything with that word in its name or path, drive names included (`!BACKUP` hides all of `/mnt/drives/BACKUP`).
- **Exact Phrase:** `"two words"` matches the phrase.
- **Wildcards:** `*.mkv` and `IMG_????` (`*` is any text, `?` is one character).
- **Extension:** `ext:pdf;epub` searches by extension.
- **Inside A Folder:** `/mnt/drives/PHOTOS/ invoice` searches only inside that folder.
- **Direct Parent:** `parent:/home/me/Documents` matches only files directly in that folder.
- **Folders And Files:** `folder:`, `file:` and `empty:` match only folders, only files, or empty folders.
- **By Kind:** `image:`, `audio:`, `video:`, `doc:`, `zip:` and `exe:` (also in the list right of the box).
- **File Content:** `content:invoice` also reads the files for that word (slower; add a name part such as `ext:txt content:invoice` to keep it quick).
- **Match Menu:** Menu > Match offers Match Case (Ctrl+I), Whole Words (Ctrl+B), Start / End / Whole Name, Ignore Punctuation and Ignore Accents. The menu ticks the ones that are on.

### The Window

- **Thumbnails:** the Grid button (Ctrl+G) shows thumbnails instead of a list; Ctrl + mouse wheel makes them bigger or smaller.
- **Preview:** Menu > Preview Pane (Alt+P) previews the selected picture.
- **Status Bar:** the left side shows the result count; in thumbnails also the selected file's folder.
- **Columns:** the three-dot button (row 2) shows or hides Folder, Size, Type and Date Modified. Click a header to sort, drag to move, right-click to hide or move.
- **Drag:** drag a result into Files, an editor or a browser upload.
- **Select:** Ctrl+click and Shift+click select several results; Ctrl+A selects all.
- **Copy And Cut:** Ctrl+C or Ctrl+X, then paste in Files (Nautilus). A cut file stays listed until the paste moves it.
- **Delete:** moves the selected files to the Trash (asks first; never deleted for good).
- **Copy Paths:** Ctrl+Shift+C copies the paths.
- **Hide:** right-click > Hide From Results (Ctrl+H) hides the selected results (a folder with everything in it). Undo in Preferences > Folders & Logs > Hidden Folders.
- **Excluded Folders:** Preferences (Ctrl+,) > Folders & Logs > Excluded Folders > Add Folder... never indexes a folder.
- **Hidden Folders:** Preferences > Folders & Logs > Hidden Folders > Add Folder... keeps a folder indexed but never shown.
- **Delete Logs:** Preferences > Folders & Logs > Log > Delete All Logs removes the daemon and window logs and saved problem reports.
- **Rescan All:** Menu > Daemon Status > Rescan All restarts the daemon (password) with no saved index and reads every drive from scratch; search waits until it is done; settings, lists and logs stay.
- **Status Button:** the right side shows a dot and "Up to date", "Indexing DATA · 3/6" or "Stopped", a small bar while indexing and an orange page icon while the daemon logs. Click it for the panel: uptime, memory, Start / Stop, indexing progress, items and folders, every drive, Debug logging and Report an issue.
- **Report An Issue:** in the panel, pick the file that should have shown. It saves a small report (a few KB) in your home folder to share, and re-reads the folder that lost it so it shows.
- **Daemon Status:** Menu > Daemon Status shows the full counters (live or timed checks, events, last save).
- **Search Words Page:** Preferences > Search Words lists the search words with examples.
- **Search Page:** Preferences > Search sets columns, Short Folder Path, alternating row colors and Search Delay (the wait after the last key before searching, 80 ms).
- **Close:** Escape closes Preferences and Daemon Status (their own windows); Ctrl+Q closes the program.

### Drives

- **Live Drives:** ext4, XFS, btrfs and NTFS with the ntfs3 driver are live: changes show at once.
- **NTFS Speed:** NTFS drives on ntfs3 are first indexed (and rebuilt) from their file table, the MFT, in one read instead of folder by folder. If that table can't be used, the log says why and the folders are read one by one as before.
- **Timed Drives:** NTFS-3g sends no change events, so those drives (and any other the daemon can't watch) are re-checked on a timer. Daemon Status shows which drives are live and how often the others are checked.

## How To Tell It Is Running

- **Window Open:** the status button (bottom right) shows a green dot and "Up to date"; click it to see how long the daemon has been up.
- **Window Closed:** the daemon keeps running and there is no icon. In a terminal, in the build folder (or `/opt/findanything` with the service), `./fa status` prints size, uptime and per-drive state when it runs, or "cannot reach the daemon ... Is it running?" when it does not.
- **Other Checks:** `pgrep -a fa`, or with the service `systemctl status findanything`. Open the window again any time from the app menu.

## Command Line

Run in the build folder. The daemon must be running, except for `daemon`.

- **Status:** `./fa status` shows whether it runs, size, uptime and per-drive LIVE / NOT LIVE.
- **Find:** `./fa find 'ext:png holiday'` searches (same words as the window); add `-n 500` for more rows.
- **Find In Files:** `./fa find ext:md content:todo` searches inside the files too.
- **Rebuild:** `./fa rebuild` starts the index over.
- **Report:** `./fa report FILE [WORDS]` writes a problem report for a file that doesn't show (same as the warning button).
- **Verify:** `./fa verify ~/Documents` checks the index against the disk for a folder (names, folders, sizes, dates) and prints the number of differences.
- **Exclude:** `./fa exclude node_modules` never indexes that folder; plain `./fa exclude` lists them; `./fa exclude --clear` empties the list.
- **Stop:** `./fa stop` stops the daemon.
- **MFT Check:** `sudo ./fa mftcheck /mnt/drives/PHOTOS` reads an NTFS drive from its MFT and folder by folder, and prints both times and any difference.
- **Daemon:** `sudo ./fa daemon` starts it without the window (add `--debug --log fa.log`).
- **Version:** `./fa --version` prints the version.
- **Window:** `./fa-gui` opens the window.

## Safety

- **Read Only:** the daemon only reads your drives and never writes to them (NTFS file tables are read from the drive opened read-only).
- **Saved Index:** it keeps its own saved index in `/var/cache/findanything/index.bin` (root only).
- **Log:** its log, `fa-<host>.log`, is in the folder it runs from (`/opt/findanything` with the service).
- **Settings:** they live in `~/.config/findanything/`.
- **Root Powers:** the service keeps only the root powers the daemon needs (watching drives, reading every folder, handing its socket and log to you) and can't gain more.

## Bug Reports

Set Preferences > Folders & Logs > Log > Detailed (applies at once), then attach these to your report:

- **Daemon Log:** `fa-<host>.log` in the build folder next to `fa-gui` (with the service: `/opt/findanything`).
- **Window Log:** `gui-debug.log` in the same folder (with the service: `~/.config/findanything/`), plus `gui-debug.log.1` to `.5`, the five runs before. After a force quit or a restart the stall is in one of those; Report a Problem lists stalls.
- **Journal:** with the service, also run `journalctl -b -u findanything > ~/findanything-journal.txt`.
- **Order:** copy them before Preferences > Folders & Logs > Delete All Logs, which deletes them.

## Uninstall

Run `sh uninstall.sh` in the build folder. It lists what it will delete and asks first.

- **Stops:** the daemon, and with the service it stops and disables it first.
- **Removes User Files:** the app folder in `~/Applications`, the settings and cache (`~/.config/findanything`, `~/.cache/findanything`), and the app-menu entry and icon.
- **Removes Root Files:** the root-owned index (`/var/cache/findanything`), `/opt/findanything` if you used it, `/etc/systemd/system/findanything.service` and the polkit rule.
- **Old Names:** it also removes the old SearchEverything names if they are there.
- **Password:** it asks for your password for the root parts.
- **Check:** afterwards `ls ~/Applications ~/.config | grep -i "find\|search"` should print nothing.
