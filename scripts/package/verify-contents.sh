#!/bin/sh
set -eu

if [ "$#" -lt 1 ]; then
    printf '%s\n' "usage: $0 <package> [...]" >&2
    exit 2
fi

contains_path() {
    input_paths=$1
    expected_path=$2

    while IFS= read -r path; do
        path=${path#./}
        path=${path#/}
        if [ "$path" = "$expected_path" ]; then
            return 0
        fi
    done <<EOF
$input_paths
EOF

    return 1
}

for package in "$@"; do
    case "$package" in
        *.deb)
            package_paths=$(dpkg-deb --fsys-tarfile "$package" | tar -tf -)
            service=lib/systemd/system/terakzor.service
            config_paths=$(dpkg-deb --ctrl-tarfile "$package" | tar -xOf - ./conffiles)
            if ! contains_path "$config_paths" etc/terakzor/terakzor.toml; then
                printf '%s does not register /etc/terakzor/terakzor.toml as a conffile\n' "$package" >&2
                exit 1
            fi
            ;;
        *.rpm)
            package_paths=$(rpm -qlp "$package")
            service=usr/lib/systemd/system/terakzor.service
            rpm_file_flags=$(rpm -qp --qf '[%{FILEFLAGS:fflags} %{FILENAMES}\n]' "$package")
            case "$rpm_file_flags" in
                *"cn /etc/terakzor/terakzor.toml"*|*"config, noreplace /etc/terakzor/terakzor.toml"*) ;;
                *)
                    printf '%s does not register /etc/terakzor/terakzor.toml as config(noreplace)\n' "$package" >&2
                    exit 1
                    ;;
            esac
            ;;
        *.apk)
            package_paths=$(tar -tf "$package")
            service=etc/init.d/terakzor
            ;;
        *.pkg.tar.zst)
            package_paths=$(tar -tf "$package")
            service=usr/lib/systemd/system/terakzor.service
            ;;
        *)
            printf 'unsupported package: %s\n' "$package" >&2
            exit 2
            ;;
    esac

    for required in usr/bin/terakzor etc/terakzor/terakzor.toml usr/share/doc/terakzor/copyright "$service"; do
        if ! contains_path "$package_paths" "$required"; then
            printf '%s is missing %s\n' "$package" "$required" >&2
            exit 1
        fi
    done
done

if [ -n "${TERAKZOR_BINARY:-}" ]; then
    case "$(file -Lb "$TERAKZOR_BINARY")" in
        *"statically linked"*) ;;
        *)
            printf '%s is not a static binary\n' "$TERAKZOR_BINARY" >&2
            exit 1
            ;;
    esac
fi
