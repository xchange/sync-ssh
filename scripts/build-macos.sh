#!/usr/bin/env bash
set -euo pipefail
# Usage: scripts/build-macos.sh [debug|release] [destination-directory]
script_directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$script_directory/bundle-macos.sh" "${2:-$script_directory/../dist}" "${1:-release}"
