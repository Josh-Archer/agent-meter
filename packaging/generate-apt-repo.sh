#!/usr/bin/env bash
set -euo pipefail

# Generates and cryptographically signs a standard Debian/Ubuntu APT repository
# for GitHub Pages hosting (https://josh-archer.github.io/agent-meter/).
#
# Layout:
#   pool/main/a/agent-meter/agent-meter_<version>_<arch>.deb
#   dists/stable/Release
#   dists/stable/InRelease
#   dists/stable/Release.gpg
#   dists/stable/main/binary-<arch>/Packages
#   dists/stable/main/binary-<arch>/Packages.gz
#   agent-meter-archive-keyring.gpg

input_dir="${1:-dist}"
output_dir="${2:-dist/apt-repo}"
arch="${3:-amd64}"
distribution="stable"
component="main"
origin="Agent Meter"
label="Agent Meter"
codename="stable"
description="Local GNOME usage meter for coding-agent harnesses"

if [[ ! -d "$input_dir" ]]; then
    echo "Input directory '$input_dir' not found" >&2
    exit 1
fi

mkdir -p "$output_dir"
abs_out=$(cd "$output_dir" && pwd)

pool_dir="$abs_out/pool/$component/a/agent-meter"
dists_dir="$abs_out/dists/$distribution"
binary_dir="$dists_dir/$component/binary-$arch"

mkdir -p "$pool_dir" "$binary_dir"

# Copy all .deb packages from input_dir to pool
found_debs=0
for deb in "$input_dir"/*.deb; do
    if [[ -f "$deb" ]]; then
        cp -f "$deb" "$pool_dir/"
        found_debs=1
    fi
done

if [[ $found_debs -eq 0 ]]; then
    echo "No .deb files found in '$input_dir'" >&2
    exit 1
fi

# Generate Packages file
# We use dpkg-scanpackages if available, or generate conforming control stanzas
cd "$abs_out"
packages_file="$binary_dir/Packages"

if command -v dpkg-scanpackages >/dev/null 2>&1; then
    dpkg-scanpackages -m "pool/$component/a/agent-meter" > "$packages_file"
else
    # Fallback standard Packages generator
    : > "$packages_file"
    for deb_file in "$pool_dir"/*.deb; do
        if [[ -f "$deb_file" ]]; then
            rel_path="pool/$component/a/agent-meter/$(basename "$deb_file")"
            size=$(stat -c%s "$deb_file" 2>/dev/null || stat -f%z "$deb_file")
            md5=$(md5sum "$deb_file" | awk '{print $1}')
            sha1=$(sha1sum "$deb_file" | awk '{print $1}')
            sha256=$(sha256sum "$deb_file" | awk '{print $1}')
            sha512=$(sha512sum "$deb_file" | awk '{print $1}')

            dpkg-deb -I "$deb_file" control > "$binary_dir/.control.tmp"
            cat "$binary_dir/.control.tmp" >> "$packages_file"
            rm -f "$binary_dir/.control.tmp"

            cat >> "$packages_file" <<EOF
Filename: $rel_path
Size: $size
MD5sum: $md5
SHA1: $sha1
SHA256: $sha256
SHA512: $sha512

EOF
        fi
    done
fi

# Generate Packages.gz
gzip -9cn "$packages_file" > "$binary_dir/Packages.gz"

# Generate Release file
cd "$dists_dir"
release_file="$dists_dir/Release"

cat > "$release_file" <<EOF
Origin: $origin
Label: $label
Suite: $distribution
Codename: $codename
Version: 1.0
Architectures: $arch
Components: $component
Description: $description
Date: $(date -Ru)
EOF

compute_hash_stanza() {
    local algo="$1"
    local header="$2"
    echo "$header:" >> "$release_file"

    local rel_pkgs="$component/binary-$arch/Packages"
    local rel_pkgs_gz="$component/binary-$arch/Packages.gz"

    for rel in "$rel_pkgs" "$rel_pkgs_gz"; do
        local file="$dists_dir/$rel"
        if [[ -f "$file" ]]; then
            local hash
            local size
            size=$(stat -c%s "$file" 2>/dev/null || stat -f%z "$file")
            case "$algo" in
                md5) hash=$(md5sum "$file" | awk '{print $1}') ;;
                sha1) hash=$(sha1sum "$file" | awk '{print $1}') ;;
                sha256) hash=$(sha256sum "$file" | awk '{print $1}') ;;
                sha512) hash=$(sha512sum "$file" | awk '{print $1}') ;;
            esac
            printf " %s %16d %s\n" "$hash" "$size" "$rel" >> "$release_file"
        fi
    done
}

compute_hash_stanza "md5" "MD5Sum"
compute_hash_stanza "sha1" "SHA1"
compute_hash_stanza "sha256" "SHA256"
compute_hash_stanza "sha512" "SHA512"

# Sign repository metadata if GPG private key is provided
keyring_out="$abs_out/agent-meter-archive-keyring.gpg"
sign_key="${APT_SIGNING_KEY:-${GPG_PRIVATE_KEY:-}}"
passphrase="${APT_SIGNING_PASSPHRASE:-${GPG_PASSPHRASE:-}}"

if [[ -n "$sign_key" ]]; then
    gpghome=$(mktemp -d)
    chmod 700 "$gpghome"
    cleanup_gpg() {
        if [[ -d "$gpghome" ]]; then
            rm -rf "$gpghome"
        fi
    }
    trap cleanup_gpg EXIT

    export GNUPGHOME="$gpghome"

    # Import signing key without tracing/echoing key material
    set +x
    if [[ -n "$passphrase" ]]; then
        printf "%s\n" "$sign_key" | gpg --batch --quiet --import 2>/dev/null || printf "%s\n" "$sign_key" | gpg --batch --import
    else
        printf "%s\n" "$sign_key" | gpg --batch --quiet --import 2>/dev/null || printf "%s\n" "$sign_key" | gpg --batch --import
    fi

    # Export public keyring
    gpg --batch --yes --export --output "$keyring_out"

    # Clearsigned InRelease and detached Release.gpg
    if [[ -n "$passphrase" ]]; then
        gpg --clearsign --digest-algo SHA512 --batch --yes --pinentry-mode loopback --passphrase "$passphrase" -o "$dists_dir/InRelease" "$release_file"
        gpg --armor --detach-sign --digest-algo SHA512 --batch --yes --pinentry-mode loopback --passphrase "$passphrase" -o "$dists_dir/Release.gpg" "$release_file"
    else
        gpg --clearsign --digest-algo SHA512 --batch --yes -o "$dists_dir/InRelease" "$release_file"
        gpg --armor --detach-sign --digest-algo SHA512 --batch --yes -o "$dists_dir/Release.gpg" "$release_file"
    fi
fi

echo "APT repository generated successfully at: $abs_out"
