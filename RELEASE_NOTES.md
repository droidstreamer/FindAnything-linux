# Find Anything release notes

## 0.22.2 (2026-11-04) — clean release
- **No Personal Paths:** the window (`fa-gui`) carried 96 build-machine paths (the home folder of whoever built it) inside it, used only in crash messages. Builds now replace that prefix, so none remain.
- **Current Version Only:** What's New shows only this version. The full history of earlier versions is kept outside the program.
- **Generic Examples:** drive and folder names in the help, the notes and the tests no longer name real drives.
- **Markdown Notes:** README, INSTALL and INSTALL_SERVICE are now `.md` files with a bold subheader on every entry (install.sh copies them under the new names).
- **Smaller Build Folder:** the check scripts (`live_check.sh`, `drive_check.sh`, `mft_check.sh`, `root_check.sh`) are no longer shipped; only `install.sh` and `uninstall.sh` are.
- **Unchanged:** the daemon and search work as in 0.22.1.
