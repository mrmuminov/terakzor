#!/bin/sh
set -eu

install -d -m 0750 -o terakzor -g terakzor /var/lib/terakzor

# Generate a secure random MCP token on first install if it is still the default
if [ -f "/etc/terakzor/terakzor.toml" ]; then
    if grep -q 'mcp_token = "dev-mcp-token-replace-me"' "/etc/terakzor/terakzor.toml"; then
        NEW_TOKEN=$(head -c 16 /dev/urandom | od -An -t x1 | tr -d ' \n')
        sed -i "s/mcp_token = \"dev-mcp-token-replace-me\"/mcp_token = \"$NEW_TOKEN\"/" "/etc/terakzor/terakzor.toml"
    fi
fi


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
