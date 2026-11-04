#!/bin/sh
# Find Anything: stop the daemon and remove everything it installed (also the old SearchEverything names).
# Run:  sh uninstall.sh        (asks before it deletes; sudo asks for your password for the root-owned parts)
HERE=$(dirname "$(realpath "$0")")
APPLIC="$HOME/Applications/FindAnything $HOME/Applications/SearchEverything"
USERF="$HOME/.config/findanything $HOME/.cache/findanything $HOME/.config/searcheverything $HOME/.cache/searcheverything
$HOME/.local/share/applications/dev.droid.FindAnything.desktop $HOME/.local/share/applications/dev.droid.SearchEverything.desktop
$HOME/.local/share/icons/hicolor/256x256/apps/dev.droid.FindAnything.png $HOME/.local/share/icons/hicolor/256x256/apps/dev.droid.SearchEverything.png"
ROOTF="/var/cache/findanything /var/cache/searcheverything /opt/findanything /opt/searcheverything /run/findanything.sock /run/findanything.sock.lock /run/searcheverything.sock /run/searcheverything.sock.lock /etc/systemd/system/findanything.service"
# install.sh's polkit rule: its folder isn't readable without root, so listed with the service.
RULE=/etc/polkit-1/rules.d/50-findanything.rules
UNIT=/etc/systemd/system/findanything.service

found=""
for p in $APPLIC $USERF $ROOTF; do [ -e "$p" ] && found="$found $p"; done
[ -e "$UNIT" ] && found="$found $RULE"
echo "Will stop the daemon and delete:"
if [ -z "$found" ]; then echo "  (nothing installed found)"; else for p in $found; do echo "  $p"; done; fi
echo "This build folder ($HERE) is deleted only if it is inside one of the folders above."
printf "Go ahead? [y/N] "; read a; [ "$a" = y ] || [ "$a" = Y ] || { echo "Nothing changed."; exit 0; }

[ -e "$UNIT" ] && sudo systemctl disable --now findanything.service
for s in "$HERE/fa" "$HOME/Applications/FindAnything"/*/fa; do [ -x "$s" ] && "$s" stop 2>/dev/null; done
for s in "$HOME/Applications/SearchEverything"/*/se; do [ -x "$s" ] && "$s" stop --socket /run/searcheverything.sock 2>/dev/null; done
sleep 1
for p in $found; do
  case "$p" in
    /var/*|/opt/*|/run/*) sudo rm -rf "$p" ;;
    /etc/*) sudo rm -f "$p" ;;
    *) rm -rf "$p" ;;
  esac
done
case "$found" in *"$UNIT"*) sudo systemctl daemon-reload ;; esac
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null
echo "Done. Find Anything is removed. (A copy still on the USB key is untouched.)"
