# Find Anything 0.22.2: Install As A Service (Ubuntu)

Every file and where it lives, plus a clean uninstall. The service runs the daemon (`fa`) from boot, before you log in, so the search window opens ready: no Start button, no password. The window (`fa-gui`) is an ordinary program you open from the app menu.

Two ways to go from 0.22.1 to 0.22.2. Pick one:

- **Update In Place:** run 0.22.2's `install.sh` (quickest). It keeps your settings, hidden list, excluded folders and saved index.
- **Clean:** uninstall 0.22.1 completely (section 4), then install 0.22.2 (section 1). Every drive is read from scratch once. Settings, the hidden list and the excluded folders are deleted too: note them first if you need them (Preferences > Folders & Logs).

## 1. Install (Fresh, Or After A Clean Uninstall)

1. **Copy:** put the build folder in your home folder:
   ```
   mkdir -p ~/Applications/FindAnything
   cp -r /path/to/FindAnything_v0.22.2 ~/Applications/FindAnything/
   ```
2. **Installer:** run it from that folder, as yourself (not with sudo):
   ```
   cd ~/Applications/FindAnything/FindAnything_v0.22.2
   sh install.sh
   ```
   It prints every step and asks "Go ahead? [y/N]", then "Add the polkit rule? [y/N]" (y = Start and Stop in the window need no password; n = they ask for it). sudo asks for your password once. It does the following:
   - **Stops The Daemon:** a running daemon saves its index first.
   - **Copies Files:** `fa`, `fa-gui`, `install.sh`, `uninstall.sh` and the notes go to `/opt/findanything` (owned by root; an existing `fa` and `fa-gui` are kept as `fa.prev` and `fa-gui.prev`).
   - **Writes The Unit:** `/etc/systemd/system/findanything.service`.
   - **Polkit Rule:** writes (y) or removes (n) `/etc/polkit-1/rules.d/50-findanything.rules`.
   - **Starts It:** `systemctl daemon-reload`, then `enable --now findanything.service`. It ends with three lines of `fa status` and "Done: the daemon runs now and starts at every boot."
3. **First Window:** open the window once from the installed copy so the app menu and dock point there: `/opt/findanything/fa-gui`. Then press Super, type "Find Anything", press Enter. To keep it in the dock, right-click its icon while it runs > Pin to Dash.
4. **Check:** follow section 3.

## 2. Update In Place (From 0.22.1)

1. **Copy:** put the 0.22.2 folder next to the old one (step 1 above).
2. **Installer:** `cd` into the 0.22.2 folder and run `sh install.sh` (y, then y or n for the polkit rule). It stops 0.22.1, keeps it as `/opt/findanything/fa.prev` and `fa-gui.prev`, installs 0.22.2 and starts it.
3. **Reopen:** close the search window and open it again from the app menu. The title bar shows Version 0.22.2.
4. **Check And Tidy:** follow section 3. The old build folder can be deleted by hand afterwards (`rm -r ~/Applications/FindAnything/FindAnything_v0.22.1`); nothing uses it.

Go back to 0.22.1 if 0.22.2 misbehaves:

```
sudo systemctl stop findanything
sudo cp -p /opt/findanything/fa.prev /opt/findanything/fa
sudo cp -p /opt/findanything/fa-gui.prev /opt/findanything/fa-gui
sudo systemctl start findanything
```

## 3. Check It Is The Right Version And Running

- **Service:** `systemctl status findanything` shows "active (running)" and since when.
- **Version:** `/opt/findanything/fa --version` prints `Find Anything 0.22.2 (Droid)`.
- **Daemon:** `/opt/findanything/fa status | head -3` shows the daemon's version, uptime and how it started.
- **Journal:** `journalctl -b -u findanything` shows the service's messages for this boot.
- **Window:** the title bar shows the version and About > What's New shows 0.22.2. If a bar says the daemon is another version, `install.sh` was not run from that build's folder.
- **Dock And Menu:** `grep '^Exec' ~/.local/share/applications/dev.droid.FindAnything.desktop` should print `Exec=/opt/findanything/fa-gui`. The window rewrites this line each time it starts, so opening `/opt/findanything/fa-gui` once fixes it. A pinned dock icon uses this same file.
- **After A Reboot:** `/opt/findanything/fa status | grep -i start` shows e.g. "ready in 9.8 s", not 966 s. `grep -E "new device number|new mount|dropping its kept copy" /opt/findanything/fa-*.log` shows the late-mount handling.

## 4. Clean Uninstall (Of Any Version)

Easiest: run the uninstall script. It lists everything it will delete and asks first.

```
sh /opt/findanything/uninstall.sh
```

The `uninstall.sh` in any build folder does the same. It stops and disables the service, stops any daemon started by hand, then deletes:

- **Service Files:** `/etc/systemd/system/findanything.service` and the polkit rule, then `systemctl daemon-reload`.
- **Program Folder:** `/opt/findanything` (`fa`, `fa-gui`, `.prev` copies, notes, daemon logs).
- **Daemon Data:** `/var/cache/findanything` (saved index, excluded folders list, drive sizes).
- **Socket:** `/run/findanything.sock` and `.lock`, if left behind.
- **Settings:** `~/.config/findanything` (window settings, hidden list, window logs).
- **Old Cache:** `~/.cache/findanything` (old versions only).
- **Menu Entry:** the app-menu entry and icon in `~/.local/share`.
- **Build Copies:** `~/Applications/FindAnything`. Careful: this removes every build folder in it, including a 0.22.2 you already copied there. Uninstall first, then copy 0.22.2.
- **Old Names:** the old SearchEverything names of all of these, if present.

It asks for your password for the root parts. It does not delete problem reports you saved (`~/FindAnything-report-*.txt`) or the shared thumbnail cache (`~/.cache/thumbnails`, also used by Files).

The same by hand, if you prefer (one line at a time; check each before the next):

```
sudo systemctl disable --now findanything.service
sudo rm -f /etc/systemd/system/findanything.service /etc/polkit-1/rules.d/50-findanything.rules
sudo systemctl daemon-reload
sudo rm -rf /opt/findanything /var/cache/findanything
sudo rm -f /run/findanything.sock /run/findanything.sock.lock
rm -rf ~/.config/findanything ~/.cache/findanything
rm -f ~/.local/share/applications/dev.droid.FindAnything.desktop
rm -f ~/.local/share/icons/hicolor/256x256/apps/dev.droid.FindAnything.png
update-desktop-database ~/.local/share/applications
rm -rf ~/Applications/FindAnything
```

The last line removes your build copies and is optional.

Check that it is all gone:

- **Unit:** `systemctl status findanything` says "Unit findanything.service could not be found."
- **Process:** `pgrep -af "fa daemon"` prints nothing.
- **Files:** `ls -d /opt/findanything /var/cache/findanything /etc/systemd/system/findanything.service ~/.config/findanything ~/.local/share/applications/dev.droid.FindAnything.desktop` says "No such file or directory" for each.
- **Dock:** an icon pinned before the uninstall may stay as a blank icon: right-click > Unpin.

## 5. Every File And Where It Lives (With The Service)

### Program (root-owned, so no program running as you can change what root runs)

- **`/opt/findanything/fa`:** the daemon and the command line.
- **`/opt/findanything/fa-gui`:** the search window.
- **`/opt/findanything/fa.prev`, `fa-gui.prev`:** the version before the last `install.sh`, for going back.
- **`/opt/findanything/install.sh`, `uninstall.sh`, `README.md`, `INSTALL.md`, `INSTALL_SERVICE.md`, `RELEASE_NOTES.md`:** copies of the build's scripts and notes.

### Service And Permissions

- **`/etc/systemd/system/findanything.service`:** runs `/opt/findanything/fa daemon --owner <you> --log /opt/findanything/fa-<host>.log --log-max-mb 5` as root at boot. It restarts after a crash (10 s); a Stop lasts until the next boot. It keeps only the root powers it needs (watch drives, read every folder, hand files to you).
- **`/etc/systemd/system/multi-user.target.wants/findanything.service`:** the link that makes it start at boot (made by `enable`, removed by `disable`).
- **`/etc/polkit-1/rules.d/50-findanything.rules`:** only if you said y; lets you start, stop and restart this one service without a password, from your own logged-in session.

### Daemon Data (root only: the index lists every file name on the computer)

- **`/var/cache/findanything/index.bin`:** the saved index (about 250 MB on a large setup); saved at every stop and every 10 minutes if anything changed.
- **`/var/cache/findanything/index.exclude`:** Excluded Folders (Preferences), never indexed.
- **`/var/cache/findanything/index.drives`:** entries per drive at the last save ("about N files").
- **`/var/cache/findanything/index.tmp`:** exists only for a moment while saving.
- **`/run/findanything.sock`:** how the window and `fa` talk to the daemon (owned by you; gone at reboot).

### Daemon Logs (owned by you, in the program folder)

- **`/opt/findanything/fa-<host>.log`:** starts, drives, timings and warnings.
- **`/opt/findanything/fa-<host>.log.1`:** the previous log, once the log reaches 5 MB.
- **`/opt/findanything/fa-<host>.log.totals`:** a small counters file, while Debug logging is on.

### Your Files (your user)

- **`~/.config/findanything/settings.txt`:** window settings (columns, sizes, search delay and more).
- **`~/.config/findanything/hidden.txt`:** Hidden Folders (indexed but never shown).
- **`~/.config/findanything/gui-debug.log`:** the window's log, plus `gui-debug.log.1` to `.5` (earlier runs). `/opt` is not writable for you, so they live here.
- **`~/.local/share/applications/dev.droid.FindAnything.desktop`:** the app-menu and dock entry.
- **`~/.local/share/icons/hicolor/256x256/apps/dev.droid.FindAnything.png`:** its icon.
- **`~/FindAnything-report-<date>.txt`:** problem reports you saved (Report an issue).
- **`~/.cache/thumbnails/`:** the shared thumbnail cache (Files uses it too; not ours alone).
- **`~/Applications/FindAnything/FindAnything_v<version>/`:** the build folders you copied, only needed to run `install.sh`; the service runs from `/opt`.

### Cleaning

- **Delete All Logs:** Preferences > Folders & Logs empties the daemon log and deletes its `.1` and `.totals`, the window logs and the saved reports.
- **Rescan All:** deletes `index.bin` and reads every drive again.
