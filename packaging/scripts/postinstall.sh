#!/bin/sh
set -eu

install -d -m 0750 -o terakzor -g terakzor /var/lib/terakzor

if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl enable terakzor.service
    if systemctl is-active --quiet terakzor.service; then
        systemctl restart terakzor.service
    else
        systemctl start terakzor.service
    fi
elif command -v rc-update >/dev/null 2>&1 && [ -d /run/openrc ]; then
    rc-update add terakzor default || true
    if rc-service terakzor status >/dev/null 2>&1; then
        rc-service terakzor restart
    else
        rc-service terakzor start
    fi
fi
