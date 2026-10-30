#!/usr/bin/env bash
# uninstall.sh — Convenience wrapper to uninstall sttme
# Usage: ./uninstall.sh [--purge]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${SCRIPT_DIR}/install.sh" --uninstall "$@"
