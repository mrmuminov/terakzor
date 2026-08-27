#!/bin/sh
set -eu

if id -u terakzor >/dev/null 2>&1; then
    exit 0
fi

if command -v apk >/dev/null 2>&1; then
    addgroup -S terakzor 2>/dev/null || true
    adduser -S -D -H -s /sbin/nologin -G terakzor terakzor
    exit 0
fi

getent group terakzor >/dev/null 2>&1 || groupadd --system terakzor
useradd --system --gid terakzor --home-dir /var/lib/terakzor --shell /usr/sbin/nologin --no-create-home terakzor
