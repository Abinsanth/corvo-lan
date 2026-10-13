#!/bin/sh
# Installs the latest corvo-lan release (https://github.com/Abinsanth/corvo-lan):
#
#   curl -fsSL https://raw.githubusercontent.com/Abinsanth/corvo-lan/release/install.sh | sh
#
# Installs to ~/.local/bin, or to $CORVO_LAN_INSTALL_DIR if set. Never uses sudo.
set -eu

REPO="Abinsanth/corvo-lan"

fail() {
    echo "corvo-lan install: $*" >&2
    exit 1
}

need() {
    command -v "$1" > /dev/null 2>&1 || fail "needs '$1', which isn't installed"
}

sha256() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum > /dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "needs 'sha256sum' or 'shasum' to verify the download"
    fi
}

# Everything runs from here, so a download cut short by `curl | sh` runs nothing.
main() {
    need curl
    need tar
    install_dir="${CORVO_LAN_INSTALL_DIR:-$HOME/.local/bin}"
    # Overridable only to test this script against a local copy of a release.
    base_url="${CORVO_LAN_BASE_URL:-https://github.com/$REPO/releases/latest/download}"

    case "$(uname -s)" in
        Darwin) os=apple-darwin ;;
        Linux) os=unknown-linux-musl ;;
        *) fail "no build for $(uname -s); build from source: https://github.com/$REPO" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=aarch64 ;;
        *) fail "no build for a $(uname -m) CPU; build from source: https://github.com/$REPO" ;;
    esac
    # A shell running under Rosetta reports x86_64 on an Apple Silicon Mac.
    if [ "$os" = apple-darwin ] && [ "$(sysctl -n hw.optional.arm64 2> /dev/null)" = 1 ]; then
        arch=aarch64
    fi

    asset="corvo-lan-$arch-$os.tar.gz"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    echo "downloading $asset"
    curl -fsSL "$base_url/$asset" -o "$tmp/$asset" || fail "couldn't download $base_url/$asset"
    curl -fsSL "$base_url/$asset.sha256" -o "$tmp/$asset.sha256" || fail "couldn't download its checksum"
    expected="$(cut -d ' ' -f 1 < "$tmp/$asset.sha256")"
    actual="$(sha256 "$tmp/$asset")"
    [ "$expected" = "$actual" ] || fail "checksum mismatch for $asset; not installing"

    tar -xzf "$tmp/$asset" -C "$tmp"
    mkdir -p "$install_dir"
    # Copy then rename, so replacing a running corvo-lan is safe.
    cp "$tmp/corvo-lan" "$install_dir/.corvo-lan.new"
    chmod 755 "$install_dir/.corvo-lan.new"
    mv -f "$install_dir/.corvo-lan.new" "$install_dir/corvo-lan"

    echo "installed $("$install_dir/corvo-lan" --version) to $install_dir"
    case ":$PATH:" in
        *":$install_dir:"*) echo "run: corvo-lan" ;;
        *)
            echo
            echo "$install_dir isn't on your PATH. Add this line to your shell profile"
            echo "(~/.zshrc or ~/.bashrc), then open a new terminal:"
            echo
            echo "    export PATH=\"$install_dir:\$PATH\""
            ;;
    esac
}

main "$@"
