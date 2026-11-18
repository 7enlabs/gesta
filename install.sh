#!/bin/sh
# Install gesta with one command.
#
#   curl -fsSL https://raw.githubusercontent.com/zencli/gesta/main/install.sh | sh
#
# Environment:
#   GESTA_VERSION   version to install, or `main` (default) for the current branch
#   GESTA_PREFIX    where the binary goes (default: ~/.local, so the binary is ~/.local/bin/gesta)
#   GESTA_FROM      `source` to build with cargo even where a prebuilt binary exists
#
# Why a shell script and not a package: the binary is one file with no runtime, so the whole
# install is a download and a chmod. A package manager would add a dependency and a delay for
# no gain on Linux and macOS.
set -eu

REPO="zencli/gesta"
VERSION="${GESTA_VERSION:-main}"
PREFIX="${GESTA_PREFIX:-$HOME/.local}"
FROM="${GESTA_FROM:-auto}"
BINDIR="$PREFIX/bin"
TARGET="$BINDIR/gesta"

say() { printf '%s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required and was not found on PATH"
}

# The asset name a release carries for this machine. Kept in one place: the release script and
# this installer must agree, or every download is a 404.
platform() {
    os=$(uname -s 2>/dev/null || echo unknown)
    arch=$(uname -m 2>/dev/null || echo unknown)
    case "$os" in
        Linux) os_part=unknown-linux-gnu ;;
        Darwin) os_part=apple-darwin ;;
        MINGW*|MSYS*|CYGWIN*) os_part=pc-windows-msvc; ext=.exe ;;
        *) die "unsupported system: $os. Build from source instead: cargo install --git https://github.com/$REPO.git" ;;
    esac
    case "$arch" in
        x86_64|amd64) arch_part=x86_64 ;;
        aarch64|arm64) arch_part=aarch64 ;;
        *) die "unsupported architecture: $arch. Build from source instead: cargo install --git https://github.com/$REPO.git" ;;
    esac
    printf '%s-%s' "$arch_part" "$os_part"
}

install_binary() {
    src="$1"
    mkdir -p "$BINDIR"
    tmp="$BINDIR/.gesta.$$"
    cp "$src" "$tmp" || die "cannot write to $BINDIR. Set GESTA_PREFIX to a directory you own."
    chmod 755 "$tmp"
    mv "$tmp" "$TARGET"
}

# A prebuilt binary needs no toolchain, so try it before asking for Rust. A machine without
# cargo can still install, which is the point of one command.
try_prebuilt() {
    [ "$FROM" = "source" ] && return 1
    plat=$(platform)
    url="https://github.com/$REPO/releases/download/$VERSION/gesta-$plat${ext:-}"
    tmpdir=$(mktemp -d)
    say "Trying a prebuilt binary for $plat"
    if curl -fsSL --retry 2 --max-time 120 -o "$tmpdir/gesta" "$url" 2>/dev/null; then
        install_binary "$tmpdir/gesta"
        rm -rf "$tmpdir"
        return 0
    fi
    rm -rf "$tmpdir"
    say "No prebuilt binary at $VERSION for $plat; building from source"
    return 1
}

build_from_source() {
    if ! command -v cargo >/dev/null 2>&1; then
        die "no prebuilt binary for this machine and no Rust to build one.
Install Rust (one command, no administrator): curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
Then run this installer again."
    fi
    # `--branch main` fails against a git remote that has no such remote-tracking ref, so the
    # default branch is requested by leaving the flag out and a version by `--tag`.
    if [ "$VERSION" = "main" ]; then
        ref="" 
    else
        ref="--tag $VERSION"
    fi
    # --locked first, so a build gets the dependency versions the project was tested with.
    # shellcheck disable=SC2086
    cargo install --git "https://github.com/$REPO.git" $ref --root "$PREFIX" --locked 2>/dev/null \
        || cargo install --git "https://github.com/$REPO.git" $ref --root "$PREFIX" \
        || die "cargo install failed. Run 'rustup update' and try again."
}

main() {
    if try_prebuilt; then :; else build_from_source; fi
    [ -x "$TARGET" ] || TARGET="$TARGET.exe"
    [ -x "$TARGET" ] || die "the binary did not land at $BINDIR"

    say ""
    say "gesta installed: $TARGET"
    case ":$PATH:" in
        *":$BINDIR:"*) say "Run: gesta --help" ;;
        *)
            say ""
            say "$BINDIR is not on PATH. Add it:"
            say "  echo 'export PATH=\"$BINDIR:\$PATH\"' >> ~/.profile && . ~/.profile"
            ;;
    esac
    say ""
    say "Then in a project:"
    say "  gesta init"
    say "  gesta changeset create \"first change\" --author me"
    say "  gesta status"
}

main
