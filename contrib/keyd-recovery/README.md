# keyd failure notification and recovery

Optional Linux/systemd + Hyprland integration. keyd failures send a persistent critical desktop notification. **Alt+§** restarts keyd without relying on keyd itself; plain **§** still types normally. Dictation reconnects within the daemon's ten-second device scan interval.

The emergency stop (Backspace + Escape + Enter) is preserved: failures do **not** automatically restart keyd. Normal stop/restart exit code 15 from keyd v2.5.0 is classified as success; panic exit 255 remains a failure.

## Install on another device

Requires an existing `keyd.service`, a running desktop notification server, `/usr/bin/notify-send`, `/usr/bin/systemctl`, sudo, and Hyprland. Configure Alt+Space → F24 separately as described in the main README. This integration does not replace your keyd keyboard mappings.

Run from the repository root **as the desktop user**, not from a root shell:

```sh
install -Dm755 contrib/keyd-recovery/keyd-recover "$HOME/.local/bin/keyd-recover"
install -Dm644 contrib/keyd-recovery/hyprland.conf "$HOME/.config/hypr/keyd-recovery.conf"

# Render the system service for this device's desktop username and numeric UID.
rendered=$(mktemp)
sed -e "s/@USER@/$(id -un)/g" -e "s/@UID@/$(id -u)/g" \
  contrib/keyd-recovery/keyd-failure-notify.service.in > "$rendered"
sudo install -m644 "$rendered" /etc/systemd/system/keyd-failure-notify.service
rm "$rendered"
sudo install -Dm644 contrib/keyd-recovery/notify-failure.conf \
  /etc/systemd/system/keyd.service.d/notify-failure.conf
sudo systemctl daemon-reload
```

Add this line **once** to a sourced Hyprland configuration file:

```ini
source = $HOME/.config/hypr/keyd-recovery.conf
```

Remove an older keyd recovery binding if present, then run `hyprctl reload`. These are configuration copies: rerun the install commands after changing the repository files. No separate enable/start command is needed for the notification service; systemd invokes it through `OnFailure`.

### Passwordless restart permission

The recovery command uses `sudo -n` and reports an error instead of prompting. If your device does not already permit passwordless restart, run `sudo visudo -f /etc/sudoers.d/keyd-recovery` and add the following, replacing `YOUR_USER` with your desktop login:

```sudoers
YOUR_USER ALL=(root) NOPASSWD: /usr/bin/systemctl restart keyd.service
```

Run `sudo visudo -c` to validate it. This grants only the exact restart command, not arbitrary systemctl/root commands. Confirm executable paths on the destination device and adjust both script and rule together if needed.

### Device-specific settings

- `section` is the § keysym on the Swiss layout. Edit the copied `hyprland.conf` for a different recovery combination; update the notification template's instructions to match.
- The notification service targets one desktop user and their `/run/user/UID/bus`. Re-render it for that user on each device. Alerts require that user's desktop notification server to be available; this is not an offline/multi-user notification queue.
- `SuccessExitStatus=15` reflects keyd v2.5.0's intentional-stop behavior. Check your installed version's behavior before retaining this setting for a different implementation.
- Existing machine-local installation used `~/.config/hypr/UserScripts/KeydRecover.sh` and a binding in `UserConfigs/UserKeybinds.conf`. When migrating that device, remove that old binding after adding the new source line; the old script can then be removed.

## Verify

```sh
hyprctl configerrors
systemctl show keyd.service -p OnFailure -p Restart -p SuccessExitStatus
# Temporarily disable remapping, then press Alt+§ to restore it:
sudo systemctl stop keyd.service
# Or recover manually:
~/.local/bin/keyd-recover
systemctl is-active keyd.service
journalctl --user -u voice-daemon.service -n 20
```

For an actual failure test, save work first, run `sudo systemctl kill --signal=SIGKILL keyd.service`, and expect a critical alert. Press Alt+§ to recover. If recovery fails, use `sudo systemctl start keyd.service`. A deliberate stop/restart should not generate a failure notification. Plain § should remain available in an editor.

## Remove

Remove the Hyprland source line and reload Hyprland. Remove the installed `keyd-recover`, `keyd-recovery.conf`, `/etc/systemd/system/keyd-failure-notify.service`, and `/etc/systemd/system/keyd.service.d/notify-failure.conf`; run `sudo systemctl daemon-reload`. Remove the dedicated sudoers rule with visudo if you added it. Do not delete other keyd overrides or keyboard mappings.
