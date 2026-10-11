#!/usr/bin/env bash
# Re-exec under bash if started by sh/dash (e.g. `sh install.sh`).
if [ -z "${BASH_VERSION:-}" ]; then
	exec bash "$0" "$@"
fi

# Installs SentryShot from an extracted release tarball.
#
#     tar -xzf sentryshot-*-x86_64-celeron.tar.gz
#     cd sentryshot-*-x86_64-celeron
#     sudo ./install.sh [INSTALL_DIR]
#
# Default INSTALL_DIR: /opt/sentryshot. Creates a systemd service "sentryshot".

set -euo pipefail

SRC_DIR=$(cd "$(dirname "$0")" && pwd)
INSTALL_DIR=${1:-/opt/sentryshot}
SERVICE_USER=${SERVICE_USER:-sentryshot}
SERVICE_NAME=sentryshot

if [ "$(id -u)" -ne 0 ]; then
	echo "error: run as root (sudo ./install.sh)" >&2
	exit 1
fi

log() { printf '==> %s\n' "$*"; }

if ! getent passwd "$SERVICE_USER" >/dev/null; then
	log "creating user $SERVICE_USER"
	useradd --system --home "$INSTALL_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
fi

log "copying files to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
cp -a "$SRC_DIR/sentryshot" "$SRC_DIR/plugins" "$SRC_DIR/libs" "$INSTALL_DIR/"
mkdir -p "$INSTALL_DIR/configs" "$INSTALL_DIR/storage"

if [ ! -f "$INSTALL_DIR/configs/sentryshot.toml" ]; then
	log "generating default config (first run)"
	# Binary writes the default config on first start; do a short probe run.
	timeout 5 "$INSTALL_DIR/sentryshot" run --config "$INSTALL_DIR/configs/sentryshot.toml" >/dev/null 2>&1 || true
fi

chown -R "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR/configs" "$INSTALL_DIR/storage"

log "installing systemd unit"
cat >"/etc/systemd/system/$SERVICE_NAME.service" <<EOF
[Unit]
Description=SentryShot NVR
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$SERVICE_USER
WorkingDirectory=$INSTALL_DIR
ExecStart=$INSTALL_DIR/sentryshot run --config $INSTALL_DIR/configs/sentryshot.toml
Restart=unless-stopped
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable "$SERVICE_NAME"

cat <<EOF

Installed to $INSTALL_DIR.

Next steps:
  1. Enable an auth plugin in $INSTALL_DIR/configs/sentryshot.toml
     (set enable = true for auth_none or auth_basic).
  2. Start the service:  systemctl start $SERVICE_NAME
  3. Logs:               journalctl -u $SERVICE_NAME -f
  UI:                    http://<server-ip>:2020/live
EOF
