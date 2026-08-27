#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
    printf 'usage: %s <package-directory>\n' "$0" >&2
    exit 2
fi

package_directory=$(realpath "$1")

verify_installation() {
    local image=$1
    local package=$2
    local install_command=$3
    local remove_command=$4

    docker run --rm -v "$package_directory:/packages:ro" "$image" sh -ec "
        $install_command /packages/$package
        id terakzor
        test -d /var/lib/terakzor
        test -f /etc/terakzor/terakzor.toml
        test -x /usr/bin/terakzor
        su -s /bin/sh terakzor -c '
            cd /var/lib/terakzor
            XDG_DATA_HOME=/var/lib /usr/bin/terakzor --config /etc/terakzor/terakzor.toml &
            pid=\$!
            sleep 1
            kill -TERM \$pid
            wait \$pid
        '
        test -e /var/lib/terakzor/terakzor.db
        $remove_command terakzor
        id terakzor
        test -e /var/lib/terakzor/terakzor.db
    "
}

verify_installation debian:9 terakzor-*-amd64.deb "dpkg -i" "dpkg -r"
verify_installation ubuntu:16.04 terakzor-*-amd64.deb "dpkg -i" "dpkg -r"
verify_installation centos:7 terakzor-*-amd64.rpm "rpm -i" "rpm -e"
verify_installation alpine:3.5 terakzor-*-amd64.apk "apk add --allow-untrusted" "apk del"
verify_installation archlinux:latest terakzor-*-amd64.pkg.tar.zst "pacman -U --noconfirm" "pacman -R --noconfirm"
