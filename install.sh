#!/usr/bin/env bash
# install.sh — Installer and uninstaller for sttme (Linux x86_64 & aarch64)
# Repository: https://github.com/maxfridbe/sttme
#
# Quick install:
#   curl -fsSL https://raw.githubusercontent.com/maxfridbe/sttme/main/install.sh | bash
#
# Local / custom install:
#   ./install.sh [--bin-dir ~/.local/bin] [--model base] [--from-source]
#
# Uninstall:
#   ./install.sh --uninstall [--purge]

set -euo pipefail

REPO="maxfridbe/sttme"
REPO_URL="https://github.com/${REPO}"
BIN_DIR="${STTME_BIN_DIR:-${HOME}/.local/bin}"
MODEL="${STTME_MODEL:-base}"
VERSION="${STTME_VERSION:-latest}"
FROM_SOURCE=0
INSTALL_SERVICE=1
UNINSTALL=0
PURGE=0

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
    BOLD=$'\033[1m'
    CYAN=$'\033[36m'
    GREEN=$'\033[32m'
    YELLOW=$'\033[33m'
    RED=$'\033[31m'
    RESET=$'\033[0m'
else
    BOLD=""
    CYAN=""
    GREEN=""
    YELLOW=""
    RED=""
    RESET=""
fi

info()  { printf '%s[sttme]%s %s\n' "${CYAN}${BOLD}" "${RESET}" "$*"; }
ok()    { printf '%s[  ok ]%s %s\n' "${GREEN}${BOLD}" "${RESET}" "$*"; }
warn()  { printf '%s[ warn]%s %s\n' "${YELLOW}${BOLD}" "${RESET}" "$*" >&2; }
err()   { printf '%s[error]%s %s\n' "${RED}${BOLD}" "${RESET}" "$*" >&2; }

usage() {
    cat <<EOF
${BOLD}sttme installer${RESET} (${REPO_URL})

Usage:
  install.sh [OPTIONS]

Options:
  --bin-dir <DIR>     Directory to install the sttme binary (default: ~/.local/bin)
  --model <NAME>      Whisper GGML model to pre-download: tiny, tiny.en, base, base.en,
                      small, medium, large-v3, large-v3-turbo, or none (default: base)
  --version <TAG>     Release tag to install, e.g. v26.1029.01 or latest (default: latest)
  --from-source       Compile sttme locally with Cargo instead of downloading a release
  --no-service        Do not create or enable the systemd --user sttme.service unit
  --uninstall         Stop daemon and remove installed binary and systemd user service
  --purge             With --uninstall, also delete ~/.config/sttme and ~/.cache/sttme
  -h, --help          Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --bin-dir)
            [[ $# -ge 2 ]] || { err "--bin-dir requires a path argument"; exit 1; }
            BIN_DIR="$2"
            shift 2
            ;;
        --model)
            [[ $# -ge 2 ]] || { err "--model requires a model name"; exit 1; }
            MODEL="$2"
            shift 2
            ;;
        --version)
            [[ $# -ge 2 ]] || { err "--version requires a release tag"; exit 1; }
            VERSION="$2"
            shift 2
            ;;
        --from-source)
            FROM_SOURCE=1
            shift
            ;;
        --no-service)
            INSTALL_SERVICE=0
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        --purge)
            PURGE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            err "Unknown option: $1"
            usage
            exit 1
            ;;
    esac
done

CONFIG_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/sttme"
CACHE_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/sttme"
SYSTEMD_USER_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user"
SERVICE_FILE="${SYSTEMD_USER_DIR}/sttme.service"

run_uninstall() {
    info "Uninstalling sttme..."

    if [[ -f "${SERVICE_FILE}" ]]; then
        if command -v systemctl >/dev/null 2>&1; then
            systemctl --user disable --now sttme.service >/dev/null 2>&1 || true
        fi
        rm -f "${SERVICE_FILE}"
        if command -v systemctl >/dev/null 2>&1; then
            systemctl --user daemon-reload >/dev/null 2>&1 || true
        fi
        ok "Removed ${SERVICE_FILE}"
    fi

    if [[ -f "${BIN_DIR}/sttme" || -L "${BIN_DIR}/sttme" ]]; then
        rm -f "${BIN_DIR}/sttme"
        ok "Removed ${BIN_DIR}/sttme"
    fi

    if [[ "${PURGE}" -eq 1 ]]; then
        rm -rf "${CONFIG_DIR}" "${CACHE_DIR}"
        ok "Purged ${CONFIG_DIR} and ${CACHE_DIR}"
    else
        info "Kept config (${CONFIG_DIR}) and cached models (${CACHE_DIR}). Pass --purge to delete them."
    fi

    ok "sttme uninstalled."
    exit 0
}

if [[ "${UNINSTALL}" -eq 1 ]]; then
    run_uninstall
fi

OS_NAME="$(uname -s)"
if [[ "${OS_NAME}" != "Linux" ]]; then
    err "sttme supports Linux only (detected: ${OS_NAME})."
    exit 1
fi

RAW_ARCH="$(uname -m)"
case "${RAW_ARCH}" in
    x86_64|amd64)
        ARCH="x86_64"
        ;;
    aarch64|arm64)
        ARCH="aarch64"
        ;;
    *)
        err "Unsupported CPU architecture '${RAW_ARCH}'. Supported: x86_64, aarch64."
        exit 1
        ;;
esac

TMP_DIR="$(mktemp -d)"
cleanup() {
    rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

fetch_url() {
    local url="$1"
    local dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 -o "${dest}" "${url}"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "${dest}" "${url}"
    else
        err "Neither 'curl' nor 'wget' is installed."
        return 1
    fi
}

build_from_source() {
    local src_dir="$1"
    if ! command -v cargo >/dev/null 2>&1; then
        err "Cargo/Rust toolchain not found. Install Rust via https://rustup.rs or download a prebuilt release from ${REPO_URL}/releases."
        exit 1
    fi
    if ! command -v cmake >/dev/null 2>&1; then
        err "'cmake' is required to compile whisper.cpp when building from source."
        exit 1
    fi

    info "Building sttme from source (release profile)..."
    (
        cd "${src_dir}"
        cargo build --release --locked
    )
    install -Dm0755 "${src_dir}/target/release/sttme" "${BIN_DIR}/sttme"
    ok "Built and installed ${BIN_DIR}/sttme"
}

clone_and_build_from_source() {
    if ! command -v git >/dev/null 2>&1; then
        err "'git' is required to clone ${REPO_URL} for source builds."
        exit 1
    fi
    local clone_dir="${TMP_DIR}/sttme-src"
    info "Cloning ${REPO_URL}..."
    if [[ "${VERSION}" != "latest" ]]; then
        git clone --depth 1 --branch "${VERSION}" "${REPO_URL}.git" "${clone_dir}"
    else
        git clone --depth 1 "${REPO_URL}.git" "${clone_dir}"
    fi
    build_from_source "${clone_dir}"
}

download_release_binary() {
    local archive_name="sttme-linux-${ARCH}.tar.gz"
    local base_download_url

    if [[ "${VERSION}" == "latest" ]]; then
        base_download_url="${REPO_URL}/releases/latest/download"
    else
        base_download_url="${REPO_URL}/releases/download/${VERSION}"
    fi

    local archive_path="${TMP_DIR}/${archive_name}"
    local sums_path="${TMP_DIR}/SHA256SUMS"

    info "Downloading prebuilt ${archive_name} (${VERSION})..."
    if ! fetch_url "${base_download_url}/${archive_name}" "${archive_path}"; then
        return 1
    fi

    if fetch_url "${base_download_url}/SHA256SUMS" "${sums_path}" 2>/dev/null; then
        if command -v sha256sum >/dev/null 2>&1; then
            info "Verifying SHA-256 checksum..."
            (
                cd "${TMP_DIR}"
                grep " ${archive_name}\$" SHA256SUMS | sha256sum -c - >/dev/null
            )
            ok "SHA-256 checksum verified"
        fi
    fi

    tar -xzf "${archive_path}" -C "${TMP_DIR}"
    if [[ ! -f "${TMP_DIR}/sttme" ]]; then
        err "Archive ${archive_name} did not contain 'sttme' binary."
        return 1
    fi

    install -Dm0755 "${TMP_DIR}/sttme" "${BIN_DIR}/sttme"
    ok "Installed prebuilt binary to ${BIN_DIR}/sttme"
    return 0
}

mkdir -p "${BIN_DIR}"

SCRIPT_DIR=""
if [[ -n "${BASH_SOURCE[0]:-}" && -f "${BASH_SOURCE[0]}" ]]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi

if [[ "${FROM_SOURCE}" -eq 1 ]]; then
    if [[ -n "${SCRIPT_DIR}" && -f "${SCRIPT_DIR}/Cargo.toml" ]]; then
        build_from_source "${SCRIPT_DIR}"
    else
        clone_and_build_from_source
    fi
elif [[ -n "${SCRIPT_DIR}" && -x "${SCRIPT_DIR}/sttme" && "${VERSION}" == "latest" ]]; then
    info "Installing bundled sttme binary from ${SCRIPT_DIR}..."
    install -Dm0755 "${SCRIPT_DIR}/sttme" "${BIN_DIR}/sttme"
    ok "Installed ${BIN_DIR}/sttme"
else
    if ! download_release_binary; then
        warn "Prebuilt release binary unavailable for '${VERSION}' (${ARCH}); falling back to source build."
        if [[ -n "${SCRIPT_DIR}" && -f "${SCRIPT_DIR}/Cargo.toml" ]]; then
            build_from_source "${SCRIPT_DIR}"
        else
            clone_and_build_from_source
        fi
    fi
fi

# Check runtime audio helper availability
if ! command -v pw-record >/dev/null 2>&1; then
    warn "'pw-record' (pipewire-utils) not found; sttme will fall back to ALSA capture."
fi

# Write default config if absent
if [[ ! -f "${CONFIG_DIR}/config.toml" ]]; then
    mkdir -p "${CONFIG_DIR}"
    cat > "${CONFIG_DIR}/config.toml" <<'EOF'
[transcription]
mode = "local"
model = "base"
language = "en"
energy_threshold = 0.008

[target]
alacritty_only = false
strip_alacritty_enter = true
trailing_space = true
input_method = "type"

[ui]
audio_beeps = true
waveform_overlay = true
minimal = false

[history]
limit = 100
EOF
    ok "Created default configuration at ${CONFIG_DIR}/config.toml"
else
    info "Preserved existing configuration at ${CONFIG_DIR}/config.toml"
fi

# Pre-download GGML Whisper model
if [[ "${MODEL}" != "none" ]]; then
    info "Ensuring Whisper '${MODEL}' GGML model is cached..."
    "${BIN_DIR}/sttme" download-model "${MODEL}"
    ok "Whisper '${MODEL}' model ready"
fi

# Configure and start systemd --user service
if [[ "${INSTALL_SERVICE}" -eq 1 ]] && command -v systemctl >/dev/null 2>&1; then
    mkdir -p "${SYSTEMD_USER_DIR}"
    cat > "${SERVICE_FILE}" <<EOF
[Unit]
Description=sttme Instant Local Whisper Speech-to-Text Daemon
Documentation=${REPO_URL}
PartOf=graphical-session.target
After=graphical-session.target pipewire.service

[Service]
Type=simple
ExecStart=${BIN_DIR}/sttme daemon
Restart=on-failure
RestartSec=1
Environment=RUST_LOG=info

[Install]
WantedBy=graphical-session.target default.target
EOF

    if systemctl --user daemon-reload >/dev/null 2>&1; then
        if systemctl --user enable sttme.service >/dev/null 2>&1 && systemctl --user restart sttme.service >/dev/null 2>&1; then
            ok "Enabled and started systemd user service (sttme.service)"
        else
            warn "Installed ${SERVICE_FILE}; start manually in a desktop session via: systemctl --user enable --now sttme.service"
        fi
    else
        warn "Installed ${SERVICE_FILE} (no active user systemd bus detected in current shell)"
    fi
fi

if [[ ":${PATH}:" != *":${BIN_DIR}:"* ]]; then
    warn "${BIN_DIR} is not currently in your PATH. Add this to ~/.bashrc or ~/.zshrc:"
    printf "    export PATH=\"%s:\$PATH\"\n" "${BIN_DIR}"
fi

printf '\n%sInstallation complete!%s\n' "${GREEN}${BOLD}" "${RESET}"
printf 'Binary:  %s/sttme\n' "${BIN_DIR}"
printf 'Config:  %s/config.toml\n\n' "${CONFIG_DIR}"

cat <<EOF
${BOLD}Wire Hold-to-Talk (F10) in Niri${RESET} (~/.config/niri/config.kdl inside binds { ... }):
    F10 repeat=true cooldown-ms=70 hotkey-overlay-title="Hold to Dictate (sttme)" { spawn "${BIN_DIR}/sttme" "hold" "--key" "f10" "--target" "all" "--quiet"; }

${BOLD}Verify setup:${RESET}
    ${BIN_DIR}/sttme doctor
    ${BIN_DIR}/sttme status
EOF
