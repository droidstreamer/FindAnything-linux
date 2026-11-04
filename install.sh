#!/bin/sh
# Find Anything: install this build as a background service that starts at boot (Ubuntu, systemd).
# Run it from the build folder, as yourself (not with sudo):  sh install.sh
# It lists every step and asks first; sudo asks for your password for the root parts.
# Run it again from a newer build folder to update. Undo: sh uninstall.sh
set -e
HERE=$(dirname "$(realpath "$0")")
DEST=/opt/findanything
UNIT=/etc/systemd/system/findanything.service
RULE=/etc/polkit-1/rules.d/50-findanything.rules
ME=$(id -un)

[ "$(id -u)" != 0 ] || { echo "Run it as yourself (sh install.sh), not with sudo: the service is set up for your user."; exit 1; }
echo "$ME" | grep -Eq '^[a-z_][a-z0-9_-]*$' || { echo "Unexpected user name: $ME"; exit 1; }
for f in fa fa-gui; do [ -x "$HERE/$f" ] || { echo "$f is missing next to install.sh"; exit 1; }; done
command -v systemctl >/dev/null || { echo "systemctl not found: this script is for Ubuntu and other systemd systems."; exit 1; }
VER=$("$HERE/fa" --version)

echo "$VER: install as a service that starts at boot."
echo "With sudo, it will:"
echo "  1. stop the daemon if it is running (it saves its index first)"
echo "  2. copy fa, fa-gui and the notes from $HERE"
echo "     to $DEST (owned by root; the previous fa and fa-gui are kept as fa.prev and fa-gui.prev)"
echo "  3. write $UNIT: runs  $DEST/fa daemon  as root at every boot;"
echo "     its socket and log ($DEST/fa-$(hostname).log) belong to $ME"
echo "  4. systemctl daemon-reload, then enable and start findanything.service"
echo "Asked next: a polkit rule so $ME can Start and Stop it in the window without a password"
echo "(No: Start asks for the password, and a rule from an earlier install.sh is removed)."
printf "Go ahead? [y/N] "; read a
case "$a" in y|Y) ;; *) echo "Nothing changed."; exit 0 ;; esac
printf "Add the polkit rule ($RULE)? [y/N] "; read p

run() { echo "+ $*"; "$@"; }

# 1. Stop whatever daemon runs (the service, or one started from a window with your password).
if systemctl is-active --quiet findanything.service; then
  run sudo systemctl stop findanything.service
else
  "$HERE/fa" stop >/dev/null 2>&1 || true
fi
i=0; while "$HERE/fa" status >/dev/null 2>&1 && [ $i -lt 120 ]; do sleep 0.5; i=$((i+1)); done

# 2. Root-owned copy: a root service must not run a file that programs running as you can change.
run sudo install -d -m 755 "$DEST"
if [ "$HERE" != "$DEST" ]; then
  if [ -f "$DEST/fa" ]; then
    run sudo cp -p "$DEST/fa" "$DEST/fa.prev"
    [ -f "$DEST/fa-gui" ] && run sudo cp -p "$DEST/fa-gui" "$DEST/fa-gui.prev"
  fi
  run sudo install -m 755 "$HERE/fa" "$HERE/fa-gui" "$HERE/install.sh" "$HERE/uninstall.sh" "$DEST/"
  for f in README.md INSTALL.md INSTALL_SERVICE.md RELEASE_NOTES.md; do
    if [ -f "$HERE/$f" ]; then run sudo install -m 644 "$HERE/$f" "$DEST/"; fi
  done
fi

# 3. The service. A clean stop (window Stop, fa stop) exits 0, so systemd leaves it stopped until
# the next boot; a crash restarts it after 10 s. Only the root powers the daemon uses are kept.
echo "+ sudo tee $UNIT"
sudo tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=Find Anything file index (instant filename search)
After=local-fs.target

[Service]
ExecStart=$DEST/fa daemon --owner $ME --log $DEST/fa-%H.log --log-max-mb 5
WorkingDirectory=$DEST
Restart=on-failure
RestartSec=10
TimeoutStopSec=120
NoNewPrivileges=yes
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE CAP_CHOWN

[Install]
WantedBy=multi-user.target
EOF

# Optional: Start/Stop without a password, for this user, this service and these verbs only.
case "$p" in
  y|Y)
    echo "+ sudo tee $RULE"
    sudo tee "$RULE" >/dev/null <<EOF
// Find Anything (install.sh): $ME may start, stop and restart findanything.service without a
// password, from a local active session. Remove with: sh uninstall.sh
polkit.addRule(function (action, subject) {
    if (action.id == "org.freedesktop.systemd1.manage-units" &&
        action.lookup("unit") == "findanything.service" &&
        subject.user == "$ME" && subject.local && subject.active &&
        ["start", "stop", "restart"].indexOf(action.lookup("verb")) >= 0) {
        return polkit.Result.YES;
    }
});
EOF
    ;;
  *) run sudo rm -f "$RULE" ;;
esac

# 4. Start it now and at every boot.
run sudo systemctl daemon-reload
run sudo systemctl enable --now findanything.service
i=0; until "$DEST/fa" status >/dev/null 2>&1 || [ $i -gt 30 ]; do sleep 0.5; i=$((i+1)); done
if "$DEST/fa" status >/dev/null 2>&1; then
  "$DEST/fa" status | head -3
  echo
  echo "Done: the daemon runs now and starts at every boot."
else
  echo "The service did not answer. See:  systemctl status findanything   and   journalctl -u findanything"
  exit 1
fi
echo "Start the window once from the installed copy, so the app menu points there:"
echo "    $DEST/fa-gui"
