#!/bin/sh
set -eu

# RPM invokes %preun with 1 and dpkg invokes prerm with upgrade during a package upgrade.
case "${1:-}" in
    1|upgrade) exit 0 ;;
esac

if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    systemctl stop terakzor.service || true
    systemctl disable terakzor.service || true
elif command -v rc-update >/dev/null 2>&1 && [ -d /run/openrc ]; then
    rc-service terakzor stop || true
    rc-update del terakzor default || true
fi
