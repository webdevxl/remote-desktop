#!/usr/bin/env bash
# LanKVM installer.
#
# Checks this Mac, installs or updates what the build needs (Rust), verifies Xcode meets the
# minimum version (and opens the App Store to update it if not), builds and signs LanKVM,
# installs it into /Applications and opens it.
#
#   ./install.sh            install or update LanKVM
#   ./install.sh --check    only report what's missing; change nothing
#
# Environment: LANKVM_INSTALL_DIR (default /Applications), LANKVM_NO_OPEN=1 to skip launching.
set -euo pipefail
cd "$(dirname "$0")"

# Minimum versions. Xcode provides Swift and the macOS SDK; Rust needs let-chains (1.88).
MIN_MACOS="14.0"
MIN_XCODE="16.0"
MIN_RUST="1.88.0"
XCODE_APP_STORE="macappstore://apps.apple.com/app/id497799835"
INSTALL_DIR="${LANKVM_INSTALL_DIR:-/Applications}"

CHECK_ONLY=false
case "${1:-}" in
    --check) CHECK_ONLY=true ;;
    "") ;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
esac

if [[ -t 1 ]]; then
    BOLD=$'\e[1m'; DIM=$'\e[2m'; GREEN=$'\e[32m'; YELLOW=$'\e[33m'; RED=$'\e[31m'; RESET=$'\e[0m'
else
    BOLD=""; DIM=""; GREEN=""; YELLOW=""; RED=""; RESET=""
fi
step() { echo; echo "${BOLD}$*${RESET}"; }
ok()   { echo "  ${GREEN}✓${RESET} $*"; }
info() { echo "  ${DIM}→${RESET} $*"; }
warn() { echo "  ${YELLOW}!${RESET} $*"; }
fail() { echo "  ${RED}✗${RESET} $*"; }
MISSING=0

# version_ge A B: true if version A >= version B (dotted numbers).
version_ge() {
    local IFS=.
    local -a a=($1) b=($2)
    local i
    for ((i = 0; i < 3; i++)); do
        local x=$((10#${a[i]:-0})) y=$((10#${b[i]:-0}))
        ((x > y)) && return 0
        ((x < y)) && return 1
    done
    return 0
}

ask() {
    # ask "question" -> true on yes (default yes); non-interactive runs answer yes.
    [[ -t 0 ]] || return 0
    local reply
    read -r -p "  $1 [Y/n] " reply
    [[ -z "$reply" || "$reply" =~ ^[Yy] ]]
}

# --- This Mac ------------------------------------------------------------------------------

step "Checking this Mac"
if [[ "$(uname -m)" != "arm64" ]]; then
    fail "LanKVM needs an Apple Silicon Mac (this one is $(uname -m))."
    exit 1
fi
MACOS="$(sw_vers -productVersion)"
if version_ge "$MACOS" "$MIN_MACOS"; then
    ok "macOS $MACOS on Apple Silicon"
else
    fail "macOS $MACOS is too old; LanKVM needs macOS $MIN_MACOS or later."
    exit 1
fi

# --- Xcode ---------------------------------------------------------------------------------

step "Checking Xcode (minimum $MIN_XCODE)"
request_xcode_update() {
    if $CHECK_ONLY; then
        info "Install or update Xcode from the App Store."
    else
        info "Opening Xcode in the App Store. Install or update it, then run ./install.sh again."
        open "$XCODE_APP_STORE" || true
    fi
}

XCODE_VERSION=""
if XCODE_INFO="$(xcodebuild -version 2>/dev/null)"; then
    XCODE_VERSION="$(awk 'NR == 1 { print $2 }' <<<"$XCODE_INFO")"
else
    # Command Line Tools alone can't build the app. Point at Xcode if it's installed.
    XCODE_APP="$(mdfind "kMDItemCFBundleIdentifier == 'com.apple.dt.Xcode'" 2>/dev/null | head -1)"
    if [[ -n "$XCODE_APP" ]]; then
        warn "The active developer directory is $(xcode-select -p 2>/dev/null || echo unset), not Xcode."
        if $CHECK_ONLY; then
            info "Run: sudo xcode-select -s \"$XCODE_APP/Contents/Developer\""
            MISSING=1
        else
            info "Switching to $XCODE_APP (macOS asks for your password)."
            sudo xcode-select -s "$XCODE_APP/Contents/Developer"
            XCODE_VERSION="$(xcodebuild -version | awk 'NR == 1 { print $2 }')"
        fi
    fi
fi

if [[ -z "$XCODE_VERSION" ]]; then
    if [[ $MISSING -eq 0 ]]; then
        fail "Xcode is not installed."
        request_xcode_update
    fi
    $CHECK_ONLY || exit 1
    MISSING=1
elif version_ge "$XCODE_VERSION" "$MIN_XCODE"; then
    ok "Xcode $XCODE_VERSION ($(xcode-select -p))"
    if ! xcodebuild -license check >/dev/null 2>&1; then
        if $CHECK_ONLY; then
            warn "The Xcode license hasn't been accepted (sudo xcodebuild -license)."
            MISSING=1
        else
            info "Accepting the Xcode license (macOS asks for your password)."
            sudo xcodebuild -license accept
        fi
    fi
else
    fail "Xcode $XCODE_VERSION is older than the required $MIN_XCODE."
    request_xcode_update
    $CHECK_ONLY || exit 1
    MISSING=1
fi

# --- Rust ----------------------------------------------------------------------------------

step "Checking Rust (minimum $MIN_RUST)"
RUSTUP_BREW_BIN="/opt/homebrew/opt/rustup/bin"
export PATH="$RUSTUP_BREW_BIN:$HOME/.cargo/bin:$PATH"

persist_path() {
    # Make Rust available in new terminals too (Homebrew's rustup isn't on PATH by default).
    local rc="$HOME/.zshrc"
    [[ "${SHELL:-}" == */bash ]] && rc="$HOME/.bash_profile"
    if ! grep -qs "opt/rustup/bin" "$rc"; then
        printf '\n# Rust (Homebrew rustup), added by LanKVM install.sh\nexport PATH="%s:$HOME/.cargo/bin:$PATH"\n' "$RUSTUP_BREW_BIN" >>"$rc"
        info "Added Rust to PATH in $rc"
    fi
}

if ! command -v rustup >/dev/null; then
    if $CHECK_ONLY; then
        fail "Rust (rustup) is not installed."
        MISSING=1
    elif command -v brew >/dev/null; then
        info "Installing rustup with Homebrew…"
        brew install rustup
        persist_path
    else
        info "Installing rustup from rustup.rs…"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    fi
fi

if command -v rustup >/dev/null; then
    if ! rustup default 2>/dev/null | grep -q .; then
        if $CHECK_ONLY; then
            fail "No default Rust toolchain."
            MISSING=1
        else
            info "Installing the stable Rust toolchain…"
            rustup default stable
        fi
    fi
    if command -v rustc >/dev/null && rustc --version >/dev/null 2>&1; then
        RUST_VERSION="$(rustc --version | awk '{ print $2 }')"
        if version_ge "$RUST_VERSION" "$MIN_RUST"; then
            ok "Rust $RUST_VERSION"
        elif $CHECK_ONLY; then
            fail "Rust $RUST_VERSION is older than $MIN_RUST (run: rustup update stable)."
            MISSING=1
        else
            info "Updating Rust $RUST_VERSION…"
            rustup update stable
            ok "Rust $(rustc --version | awk '{ print $2 }')"
        fi
    fi
    [[ -x "$RUSTUP_BREW_BIN/rustup" ]] && ! $CHECK_ONLY && persist_path
fi

# --- Code signing --------------------------------------------------------------------------

step "Checking code signing"
if security find-identity -p codesigning 2>/dev/null | grep -qE '"(lankvm-dev|Apple Development[^"]*)"'; then
    ok "Signing identity: $(security find-identity -p codesigning | awk -F'"' '/Apple Development|lankvm-dev/ { print $2; exit }')"
elif $CHECK_ONLY; then
    warn "No signing identity. macOS would forget LanKVM's permissions after each update."
    info "Create one with ./scripts/create-dev-cert.sh"
else
    warn "No signing identity. Without one, macOS forgets LanKVM's permissions after each update."
    if ask "Create a local signing certificate now?"; then
        ./scripts/create-dev-cert.sh
    fi
fi

if $CHECK_ONLY; then
    echo
    if [[ $MISSING -eq 0 ]]; then
        echo "${GREEN}Ready to install.${RESET} Run ./install.sh"
    else
        echo "${YELLOW}Some requirements are missing.${RESET} ./install.sh installs what it can."
    fi
    exit $MISSING
fi

# --- Build and install ---------------------------------------------------------------------

step "Building LanKVM"
./scripts/bundle.sh

step "Installing into $INSTALL_DIR"
if [[ ! -w "$INSTALL_DIR" ]]; then
    INSTALL_DIR="$HOME/Applications"
    mkdir -p "$INSTALL_DIR"
    warn "No write access to /Applications; installing into $INSTALL_DIR"
fi
if pgrep -xq LanKVM; then
    info "Quitting the running LanKVM…"
    osascript -e 'quit app "LanKVM"' >/dev/null 2>&1 || pkill -x LanKVM || true
    for _ in {1..20}; do pgrep -xq LanKVM || break; sleep 0.25; done
fi
rm -rf "$INSTALL_DIR/LanKVM.app"
ditto target/release/LanKVM.app "$INSTALL_DIR/LanKVM.app"
ok "Installed $INSTALL_DIR/LanKVM.app"

if [[ "${LANKVM_NO_OPEN:-}" != "1" ]]; then
    open "$INSTALL_DIR/LanKVM.app"
    ok "Opened LanKVM"
fi

echo
echo "${BOLD}Done.${RESET} If This Mac shows ${BOLD}Needs permission${RESET}, follow the steps on that page once."
