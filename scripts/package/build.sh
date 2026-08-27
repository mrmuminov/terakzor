#!/bin/sh
set -eu

: "${TERAKZOR_ARCH:?TERAKZOR_ARCH is required}"
: "${TERAKZOR_BINARY:?TERAKZOR_BINARY is required}"
: "${TERAKZOR_VERSION:?TERAKZOR_VERSION is required}"

output_dir=${1:-dist/packages}
mkdir -p "$output_dir"

for packager in deb rpm apk archlinux; do
    case "$packager" in
        deb) extension=deb ;;
        rpm) extension=rpm ;;
        apk) extension=apk ;;
        archlinux) extension=pkg.tar.zst ;;
    esac

    nfpm package --config packaging/nfpm.yaml --packager "$packager" \
        --target "$output_dir/terakzor-${TERAKZOR_VERSION}-${TERAKZOR_ARCH}.${extension}"
done
