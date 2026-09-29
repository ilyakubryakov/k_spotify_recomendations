#!/bin/sh
# spotify-agent installer — Linux and macOS, strictly user-local.
#
# Installs to ~/.local/bin (override with --prefix). Nothing here needs root:
# no system directories are touched, no package manager is invoked, and the
# script refuses to run under sudo so a stray `sudo sh install.sh` cannot
# leave root-owned files in your home directory.
#
#   curl -fsSL .../install.sh | sh                  # download a release build
#   curl -fsSL .../install.sh | sh -s -- --add-path
#   sh install.sh --prefix ~/opt       # install to ~/opt/bin
#   sh install.sh --from-source        # build locally instead of downloading
#   sh install.sh --schedule daily     # register a user timer / launch agent
#   sh install.sh --uninstall          # remove binary, schedule and shell entry
#
# Re-running it upgrades in place.
#
# POSIX sh on purpose: macOS ships bash 3.2, and some minimal Linux images have
# no bash at all. It is also written to survive being piped into `sh`, where
# there is no script file on disk to read anything else from.

set -eu

APP="spotify-agent"
PREFIX="${SPOTIFY_AGENT_PREFIX:-$HOME/.local}"
SCHEDULE=""
ADD_PATH=0
UNINSTALL=0
FORCE_SOURCE=0
REPO="${SPOTIFY_AGENT_REPO:-balancy/spotify-agent}"
# "latest" resolves through GitHub's /releases/latest/download/ redirect;
# anything else is treated as a tag.
VERSION="${SPOTIFY_AGENT_VERSION:-latest}"
ASSET_BASE="${SPOTIFY_AGENT_ASSET_BASE:-}"
SKIP_VERIFY="${SPOTIFY_AGENT_SKIP_VERIFY:-0}"

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

# --------------------------------------------------------------------------
# output helpers
# --------------------------------------------------------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_OK=$(printf '\033[32m'); C_WARN=$(printf '\033[33m')
    C_ERR=$(printf '\033[31m'); C_DIM=$(printf '\033[2m'); C_OFF=$(printf '\033[0m')
else
    C_OK=''; C_WARN=''; C_ERR=''; C_DIM=''; C_OFF=''
fi

say()  { printf '%s\n' "$*"; }
info() { printf '  %s\n' "$*"; }
ok()   { printf '%s✓%s %s\n' "$C_OK" "$C_OFF" "$*"; }
warn() { printf '%s!%s %s\n' "$C_WARN" "$C_OFF" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$C_ERR" "$C_OFF" "$*" >&2; exit 1; }

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

# --------------------------------------------------------------------------
# arguments
# --------------------------------------------------------------------------
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)     PREFIX="${2:?--prefix needs a path}"; shift 2 ;;
        --prefix=*)   PREFIX="${1#*=}"; shift ;;
        --schedule)   SCHEDULE="${2:?--schedule needs daily|weekly|hourly}"; shift 2 ;;
        --schedule=*) SCHEDULE="${1#*=}"; shift ;;
        --add-path)   ADD_PATH=1; shift ;;
        --uninstall)  UNINSTALL=1; shift ;;
        --from-source) FORCE_SOURCE=1; shift ;;
        --repo)       REPO="${2:?--repo needs owner/name}"; shift 2 ;;
        --repo=*)     REPO="${1#*=}"; shift ;;
        --version)    VERSION="${2:?--version needs a tag}"; shift 2 ;;
        --version=*)  VERSION="${1#*=}"; shift ;;
        --no-verify)  SKIP_VERIFY=1; shift ;;
        -h|--help)    usage ;;
        *)            die "unknown option: $1 (try --help)" ;;
    esac
done

BIN_DIR="$PREFIX/bin"
TARGET="$BIN_DIR/$APP"

# --------------------------------------------------------------------------
# refuse root
# --------------------------------------------------------------------------
# Running as root would create root-owned files under $HOME and, worse, put the
# OAuth token store somewhere the normal user cannot read — so the next run
# would silently ask you to log in again.
if [ "$(id -u)" = "0" ] && [ -z "${SPOTIFY_AGENT_ALLOW_ROOT:-}" ]; then
    die "do not run this installer as root or with sudo; $APP is a per-user tool
       (set SPOTIFY_AGENT_ALLOW_ROOT=1 only if you genuinely want a root-owned install)"
fi

# --------------------------------------------------------------------------
# platform detection
# --------------------------------------------------------------------------
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
    Linux)  PLATFORM=linux ;;
    Darwin) PLATFORM=macos ;;
    *)      die "unsupported OS '$OS'. Windows users: run packaging/install.ps1 in PowerShell." ;;
esac
case "$ARCH" in
    x86_64|amd64)  TRIPLE_ARCH=x86_64 ;;
    arm64|aarch64) TRIPLE_ARCH=aarch64 ;;
    *)             TRIPLE_ARCH="$ARCH" ;;
esac

case "$PLATFORM" in
    linux)
        # Alpine and friends have no glibc, so the gnu build simply will not
        # run there. The musl build is fully static and runs anywhere.
        if (ldd --version 2>&1 || true) | grep -qi musl; then
            TRIPLE="$TRIPLE_ARCH-unknown-linux-musl"
        else
            TRIPLE="$TRIPLE_ARCH-unknown-linux-gnu"
        fi
        CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/$APP"
        DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/$APP" ;;
    macos)
        TRIPLE="$TRIPLE_ARCH-apple-darwin"
        CONFIG_DIR="$HOME/Library/Application Support/$APP"
        DATA_DIR="$CONFIG_DIR" ;;
esac

SYSTEMD_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
LAUNCH_AGENTS="$HOME/Library/LaunchAgents"
LAUNCH_LABEL="io.github.spotify-agent"

# --------------------------------------------------------------------------
# uninstall
# --------------------------------------------------------------------------
remove_schedule() {
    if [ "$PLATFORM" = linux ]; then
        if command -v systemctl >/dev/null 2>&1; then
            systemctl --user disable --now "$APP.timer" >/dev/null 2>&1 || true
        fi
        rm -f "$SYSTEMD_DIR/$APP.service" "$SYSTEMD_DIR/$APP.timer"
        if command -v systemctl >/dev/null 2>&1; then
            systemctl --user daemon-reload >/dev/null 2>&1 || true
        fi
    else
        PLIST="$LAUNCH_AGENTS/$LAUNCH_LABEL.plist"
        if [ -f "$PLIST" ]; then
            launchctl unload "$PLIST" >/dev/null 2>&1 || true
            rm -f "$PLIST"
        fi
    fi
}

if [ "$UNINSTALL" = 1 ]; then
    say "Uninstalling $APP…"
    remove_schedule
    [ -f "$TARGET" ] && rm -f "$TARGET" && ok "removed $TARGET" || info "no binary at $TARGET"
    for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile" "$HOME/.config/fish/config.fish"; do
        [ -f "$rc" ] || continue
        if grep -q "# added by $APP installer" "$rc" 2>/dev/null; then
            tmp=$(mktemp) || die "cannot create a temp file"
            # `|| true` matters: when the marker is the file's ONLY line, grep -v
            # emits nothing and exits 1, which would skip the mv and silently
            # leave the entry in place.
            grep -v "# added by $APP installer" "$rc" > "$tmp" 2>/dev/null || true
            if mv "$tmp" "$rc"; then
                ok "cleaned PATH entry from $rc"
            else
                rm -f "$tmp"
                warn "could not rewrite $rc; remove the '$APP installer' line by hand"
            fi
        fi
    done
    say ""
    info "Your config and cache were left in place:"
    info "  $CONFIG_DIR"
    info "  $DATA_DIR"
    info "Delete them by hand if you want a clean slate."
    exit 0
fi

# --------------------------------------------------------------------------
# obtain the binary
# --------------------------------------------------------------------------
mkdir -p "$BIN_DIR" || die "cannot create $BIN_DIR"
[ -w "$BIN_DIR" ] || die "$BIN_DIR is not writable by $(id -un)"

fetch() {
    url="$1"; out="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$out" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$out" "$url"
    else
        return 1
    fi
}

# Where the release archives live. Overridable so a fork or a mirror works
# without editing the script.
release_base() {
    if [ -n "$ASSET_BASE" ]; then
        printf '%s' "$ASSET_BASE"
    elif [ "$VERSION" = latest ]; then
        printf 'https://github.com/%s/releases/latest/download' "$REPO"
    else
        printf 'https://github.com/%s/releases/download/%s' "$REPO" "$VERSION"
    fi
}

# Verify the archive against the release's SHA256SUMS.
#
# This is the security boundary for `curl | sh`: without it, anything that can
# intercept the download chooses what you execute. A missing checksum tool is
# reported, not skipped silently.
verify_checksum() {
    dir="$1"; archive="$2"
    [ "$SKIP_VERIFY" = 1 ] && { warn "checksum verification disabled"; return 0; }

    if ! fetch "$(release_base)/SHA256SUMS" "$dir/SHA256SUMS"; then
        warn "no SHA256SUMS published for this release; cannot verify the download"
        return 1
    fi

    expected=$(awk -v file="$archive" '$2 == file || $2 == "*"file { print $1 }' "$dir/SHA256SUMS" | head -n 1)
    if [ -z "$expected" ]; then
        warn "$archive is not listed in SHA256SUMS"
        return 1
    fi

    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$dir/$archive" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$dir/$archive" | awk '{print $1}')
    else
        warn "no sha256sum or shasum available; cannot verify the download"
        return 1
    fi

    if [ "$expected" != "$actual" ]; then
        die "checksum mismatch for $archive
       expected $expected
       actual   $actual
       Refusing to install. This is either a corrupted download or tampering."
    fi
    ok "checksum verified"
    return 0
}

install_from_release() {
    archive="$APP-$TRIPLE.tar.gz"
    url="$(release_base)/$archive"
    tmp=$(mktemp -d) || return 1

    say "Downloading $archive…"
    if ! fetch "$url" "$tmp/$archive"; then
        info "no prebuilt binary at $url"
        rm -rf "$tmp"; return 1
    fi

    if ! verify_checksum "$tmp" "$archive"; then
        # A download we cannot verify is not installed. Building from source is
        # the safe fallback, and `--no-verify` is there for air-gapped mirrors.
        rm -rf "$tmp"; return 1
    fi

    tar -xzf "$tmp/$archive" -C "$tmp" || { rm -rf "$tmp"; return 1; }
    found=$(find "$tmp" -type f -name "$APP" -perm -u+x 2>/dev/null | head -n 1)
    [ -n "$found" ] || { rm -rf "$tmp"; return 1; }
    install -m 0755 "$found" "$TARGET" 2>/dev/null || { cp "$found" "$TARGET" && chmod 0755 "$TARGET"; }
    rm -rf "$tmp"
    return 0
}

install_from_source() {
    if [ ! -f "$REPO_DIR/Cargo.toml" ]; then
        # Piped into `sh`, there is no checkout to build from.
        info "no source checkout here; to build from source:"
        info "  git clone https://github.com/$REPO && cd $APP && sh packaging/install.sh --from-source"
        return 1
    fi

    if ! command -v cargo >/dev/null 2>&1; then
        # rustup installs entirely under $HOME — still no root required.
        warn "cargo was not found."
        say  "Install the Rust toolchain (user-local, no root) with:"
        say  "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
        say  "then re-run this installer."
        return 1
    fi

    # rusqlite is built with the bundled SQLite amalgamation, so a C compiler is
    # required — but nothing is linked from the system, which is what makes the
    # resulting binary portable.
    if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
        warn "no C compiler found; SQLite is compiled from source."
        [ "$PLATFORM" = macos ] && say "    xcode-select --install" || say "    install your distro's build-essential / base-devel package"
        return 1
    fi

    say "Building from source (this takes a few minutes the first time)…"
    ( cd "$REPO_DIR" && cargo build --release --locked 2>&1 | tail -5 ) || \
        ( cd "$REPO_DIR" && cargo build --release ) || return 1

    built="$REPO_DIR/target/release/$APP"
    [ -f "$built" ] || return 1
    install -m 0755 "$built" "$TARGET" 2>/dev/null || { cp "$built" "$TARGET" && chmod 0755 "$TARGET"; }
    return 0
}

say "Installing $APP for $(id -un) on $PLATFORM/$TRIPLE_ARCH"
info "prefix  $PREFIX"
info "config  $CONFIG_DIR"
info "data    $DATA_DIR"
say ""

installed=0
if [ "$FORCE_SOURCE" = 0 ] && install_from_release; then
    installed=1
elif install_from_source; then
    installed=1
fi
[ "$installed" = 1 ] || die "could not install $APP — see the messages above"

ok "installed $TARGET"
"$TARGET" --version 2>/dev/null | sed 's/^/  /' || true

# --------------------------------------------------------------------------
# config scaffold
# --------------------------------------------------------------------------
mkdir -p "$CONFIG_DIR" "$DATA_DIR"
chmod 700 "$CONFIG_DIR" "$DATA_DIR" 2>/dev/null || true
if [ ! -f "$CONFIG_DIR/config.toml" ]; then
    "$TARGET" config init >/dev/null 2>&1 && ok "wrote $CONFIG_DIR/config.toml" || \
        warn "could not write a starter config; run '$APP config init' yourself"
fi

# --------------------------------------------------------------------------
# PATH
# --------------------------------------------------------------------------
case ":$PATH:" in
    *":$BIN_DIR:"*) ON_PATH=1 ;;
    *)              ON_PATH=0 ;;
esac

if [ "$ON_PATH" = 0 ]; then
    if [ "$ADD_PATH" = 1 ]; then
        shell_name=$(basename "${SHELL:-sh}")
        case "$shell_name" in
            zsh)  rc="$HOME/.zshrc";  line="export PATH=\"$BIN_DIR:\$PATH\"" ;;
            bash) rc="$HOME/.bashrc"; line="export PATH=\"$BIN_DIR:\$PATH\"" ;;
            fish) rc="$HOME/.config/fish/config.fish"; line="fish_add_path $BIN_DIR" ;;
            *)    rc="$HOME/.profile"; line="export PATH=\"$BIN_DIR:\$PATH\"" ;;
        esac
        mkdir -p "$(dirname "$rc")"
        printf '%s # added by %s installer\n' "$line" "$APP" >> "$rc"
        ok "added $BIN_DIR to PATH in $rc"
        info "open a new shell, or run:  . $rc"
    else
        warn "$BIN_DIR is not on your PATH."
        info "add it with:   sh $0 --add-path"
        info "or by hand:    export PATH=\"$BIN_DIR:\$PATH\""
    fi
fi

# --------------------------------------------------------------------------
# scheduling (user-level only)
# --------------------------------------------------------------------------
schedule_linux() {
    interval="$1"
    case "$interval" in
        hourly) on_calendar="hourly" ;;
        daily)  on_calendar="*-*-* 07:30:00" ;;
        weekly) on_calendar="Mon *-*-* 07:30:00" ;;
        *)      die "unknown schedule '$interval' (use hourly, daily or weekly)" ;;
    esac

    command -v systemctl >/dev/null 2>&1 || { warn "systemd not available; skipping scheduling"; return 0; }
    mkdir -p "$SYSTEMD_DIR"

    cat > "$SYSTEMD_DIR/$APP.service" <<UNIT
[Unit]
Description=spotify-agent — generate an AI-curated playlist
Documentation=https://github.com/balancy/spotify-agent
# A laptop that is offline at 07:30 should run when it reconnects, not fail.
After=network-online.target

[Service]
Type=oneshot
ExecStart=$TARGET --cron generate
# Exit 75 is EX_TEMPFAIL (rate limited / upstream down) — worth one retry.
SuccessExitStatus=0
Restart=on-failure
RestartSec=15min
# The agent needs the network and the user's home; nothing else.
PrivateTmp=true
NoNewPrivileges=true
UNIT

    cat > "$SYSTEMD_DIR/$APP.timer" <<UNIT
[Unit]
Description=Run spotify-agent on a schedule

[Timer]
OnCalendar=$on_calendar
# Fire on next boot if the machine was asleep at the scheduled time.
Persistent=true
# Spread load so every install does not hit the APIs on the same second.
RandomizedDelaySec=15min

[Install]
WantedBy=timers.target
UNIT

    systemctl --user daemon-reload >/dev/null 2>&1 || true
    if systemctl --user enable --now "$APP.timer" >/dev/null 2>&1; then
        ok "enabled the $interval user timer (no root)"
        info "status:  systemctl --user status $APP.timer"
        info "logs:    journalctl --user -u $APP.service"
        # Without lingering, user timers stop when the last session ends.
        if command -v loginctl >/dev/null 2>&1; then
            if ! loginctl show-user "$(id -un)" -p Linger 2>/dev/null | grep -q 'Linger=yes'; then
                warn "user timers only run while you are logged in."
                info "to run headless too (needs root once): sudo loginctl enable-linger $(id -un)"
            fi
        fi
    else
        warn "could not enable the timer; the unit files are in $SYSTEMD_DIR"
    fi
}

schedule_macos() {
    interval="$1"
    case "$interval" in
        hourly) sec=3600 ;;
        daily)  sec=86400 ;;
        weekly) sec=604800 ;;
        *)      die "unknown schedule '$interval' (use hourly, daily or weekly)" ;;
    esac

    mkdir -p "$LAUNCH_AGENTS" "$DATA_DIR/logs"
    PLIST="$LAUNCH_AGENTS/$LAUNCH_LABEL.plist"

    # A LaunchAgent in ~/Library/LaunchAgents runs as the logged-in user and
    # needs no admin rights, unlike a LaunchDaemon in /Library.
    cat > "$PLIST" <<PLISTEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$LAUNCH_LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$TARGET</string>
        <string>--cron</string>
        <string>generate</string>
    </array>
    <key>StartInterval</key><integer>$sec</integer>
    <!-- Catch up after the Mac was asleep instead of skipping the run. -->
    <key>RunAtLoad</key><false/>
    <key>StandardOutPath</key><string>$DATA_DIR/logs/agent.log</string>
    <key>StandardErrorPath</key><string>$DATA_DIR/logs/agent.err.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key><string>$BIN_DIR:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
    <key>ProcessType</key><string>Background</string>
</dict>
</plist>
PLISTEOF

    launchctl unload "$PLIST" >/dev/null 2>&1 || true
    if launchctl load "$PLIST" >/dev/null 2>&1; then
        ok "loaded the $interval LaunchAgent (no admin rights used)"
        info "logs:  $DATA_DIR/logs/agent.log"
        info "stop:  launchctl unload $PLIST"
    else
        warn "could not load $PLIST; load it manually with: launchctl load $PLIST"
    fi
    warn "ANTHROPIC_API_KEY must be visible to launchd."
    info "set it with:  launchctl setenv ANTHROPIC_API_KEY \"\$ANTHROPIC_API_KEY\""
    info "or put it in the config file at $CONFIG_DIR/config.toml (chmod 600)."
}

if [ -n "$SCHEDULE" ]; then
    say ""
    case "$PLATFORM" in
        linux) schedule_linux "$SCHEDULE" ;;
        macos) schedule_macos "$SCHEDULE" ;;
    esac
fi

# --------------------------------------------------------------------------
# next steps
# --------------------------------------------------------------------------
say ""
say "${C_DIM}Next steps:${C_OFF}"
say "  1. Create a Spotify app at https://developer.spotify.com/dashboard"
say "     and add this redirect URI:  http://127.0.0.1:8888/callback"
say "  2. Put the client id in $CONFIG_DIR/config.toml (or export SPOTIFY_CLIENT_ID)"
say "  3. export ANTHROPIC_API_KEY=…"
say "  4. $APP login"
say "  5. $APP            ${C_DIM}# interactive TUI${C_OFF}"
