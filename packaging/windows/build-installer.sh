#!/usr/bin/env bash
# Build sttme Windows Inno Setup installer using Docker or Podman
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

VERSION="${1:-}"
if [[ -z "${VERSION}" ]]; then
  if [[ -n "${STTME_VERSION:-}" ]]; then
    VERSION="${STTME_VERSION}"
  else
    VERSION="$(grep '^version =' "${REPO_ROOT}/Cargo.toml" | head -n1 | cut -d'"' -f2)"
  fi
fi
# Strip leading 'v' for Inno Setup numeric-friendly version string
CLEAN_VERSION="${VERSION#v}"

CONTAINER_CMD=""
if command -v docker >/dev/null 2>&1; then
  CONTAINER_CMD="docker"
elif command -v podman >/dev/null 2>&1; then
  CONTAINER_CMD="podman"
else
  echo "Error: Neither docker nor podman is installed." >&2
  exit 1
fi

echo "==> Building sttme Windows installer (v${CLEAN_VERSION}) using ${CONTAINER_CMD}..."

EXTRA_ARGS=()
if [[ "${CONTAINER_CMD}" == "podman" ]]; then
  EXTRA_ARGS+=("--userns=keep-id" "-v" "${REPO_ROOT}:/work:z")
else
  EXTRA_ARGS+=("-v" "${REPO_ROOT}:/work")
fi

mkdir -p "${REPO_ROOT}/dist/sttme-windows-x86_64"

# Ensure sttme.exe exists in the expected stage path
if [[ ! -f "${REPO_ROOT}/dist/sttme-windows-x86_64/sttme.exe" ]]; then
  echo "==> Locating sttme.exe or release zip..."
  FOUND_EXE=$(find "${REPO_ROOT}" -name "sttme.exe" -not -path "*/target/debug/*" | head -n1 || true)
  if [[ -n "${FOUND_EXE}" && -f "${FOUND_EXE}" ]]; then
    echo "Found executable at ${FOUND_EXE}"
    cp "${FOUND_EXE}" "${REPO_ROOT}/dist/sttme-windows-x86_64/sttme.exe"
  else
    FOUND_ZIP=$(find "${REPO_ROOT}" -name "sttme-windows-*.zip" | head -n1 || true)
    if [[ -n "${FOUND_ZIP}" && -f "${FOUND_ZIP}" ]]; then
      echo "Extracting from archive ${FOUND_ZIP}"
      unzip -o "${FOUND_ZIP}" -d "${REPO_ROOT}/dist/sttme-windows-x86_64/"
    fi
  fi
fi

if [[ ! -f "${REPO_ROOT}/dist/sttme-windows-x86_64/sttme.exe" ]]; then
  echo "Error: sttme.exe not found in ${REPO_ROOT}/dist/sttme-windows-x86_64/" >&2
  exit 1
fi

# Ensure container user (xclient: uid=1000) can write build outputs to dist
chmod -R 777 "${REPO_ROOT}/dist"

"${CONTAINER_CMD}" run --rm \
  "${EXTRA_ARGS[@]}" \
  -w /work \
  amake/innosetup packaging/windows/sttme.iss \
  "/DAppVersion=${CLEAN_VERSION}" \
  "/DSourceExe=dist/sttme-windows-x86_64/sttme.exe" \
  "/DOutputDir=dist" \
  "/DOutputBaseFilename=sttme-windows-x86_64-setup"

SETUP_EXE="${REPO_ROOT}/dist/sttme-windows-x86_64-setup.exe"
if [[ -f "${SETUP_EXE}" ]]; then
  SETUP_SHA=$(sha256sum "${SETUP_EXE}" | cut -d' ' -f1)
  echo "${SETUP_SHA}  sttme-windows-x86_64-setup.exe" > "${SETUP_EXE}.sha256"
  echo "==> Successfully created Windows installer:"
  ls -lh "${SETUP_EXE}"
  echo "==> SHA-256: ${SETUP_SHA}"
else
  echo "Error: ${SETUP_EXE} was not generated." >&2
  exit 1
fi
