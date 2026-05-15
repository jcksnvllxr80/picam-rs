#!/bin/bash
# Run as pi on the Raspberry Pi to install the PiCam kiosk service.
set -e

BINARY="$HOME/picam-rs/target/release/picam"

if [ ! -f "$BINARY" ]; then
    echo "ERROR: Binary not found at $BINARY — run 'cargo build --release' first."
    exit 1
fi

echo "=== Installing PiCam kiosk ==="

# ── sudo permissions for shutdown/reboot ──────────────────────────────────────
echo "pi ALL=(ALL) NOPASSWD: /sbin/shutdown, /sbin/reboot" \
    | sudo tee /etc/sudoers.d/picam-power > /dev/null
sudo chmod 440 /etc/sudoers.d/picam-power

# ── Disable lightdm (no longer needed) ───────────────────────────────────────
if systemctl is-active --quiet lightdm; then
    echo "Disabling lightdm..."
    sudo systemctl disable --now lightdm
fi

# ── Enable getty on tty1 so systemd-logind works ─────────────────────────────
sudo systemctl enable getty@tty1.service

# ── Install and enable the picam service ─────────────────────────────────────
sudo install -m 644 "$HOME/picam-rs/setup/picam.service" /etc/systemd/system/picam.service
sudo systemctl daemon-reload
sudo systemctl enable picam.service

echo ""
echo "Done. Reboot to start kiosk: sudo reboot"
echo "To check status after reboot: sudo systemctl status picam"
echo "To view logs: journalctl -u picam -f"
