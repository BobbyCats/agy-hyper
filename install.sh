#!/usr/bin/env bash
set -euo pipefail

# AGY Hyper — Zero-Barrier Installer (macOS & Linux)
# Verifies SHA-256 checksums before touching ~/.gemini/config/plugins/agy-hyper.
# Never modifies ~/.gemini/config/config.json directly.

REPO="${AGY_HYPER_REPO:-BobbyCats/agy-hyper}"
VERSION="${AGY_HYPER_VERSION:-latest}"
PLUGIN_DIR="${HOME}/.gemini/config/plugins/agy-hyper"
BIN_LINK="${HOME}/.local/bin/agy-hyper"

printf "\n⚡ Installing AGY Hyper (%s)...\n" "$VERSION"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$OS" in
  darwin) TARGET_OS="apple-darwin" ;;
  linux)  TARGET_OS="unknown-linux-musl" ;;
  *) echo "Unsupported OS: $OS"; exit 1 ;;
esac
case "$ARCH" in
  arm64|aarch64) TARGET_ARCH="aarch64" ;;
  x86_64|amd64)  TARGET_ARCH="x86_64" ;;
  *) echo "Unsupported arch: $ARCH"; exit 1 ;;
esac

TARGET="${TARGET_ARCH}-${TARGET_OS}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

recycle_sidecar() {
  # Gracefully recycle our running hyper_deck sidecar (if any) so Antigravity's
  # restart_policy="always" reloads updated routes/UI immediately without stale 404s.
  local pid_file="${AGY_HYPER_DATA_DIR:-${HOME}/.gemini/antigravity/agy-hyper}/run/sidecar.pid"
  if [[ -f "$pid_file" ]]; then
    local old_pid
    old_pid="$(cat "$pid_file" 2>/dev/null || true)"
    if [[ -n "$old_pid" ]] && kill -0 "$old_pid" 2>/dev/null; then
      if ps -p "$old_pid" -o args= 2>/dev/null | grep -q "main\.mjs"; then
        kill "$old_pid" 2>/dev/null || true
      fi
    fi
  fi
}

# Support local repo install when run from inside a clone (safe under `curl ... | bash` with `set -u`)
SCRIPT_SRC="${BASH_SOURCE[0]:-}"
SCRIPT_DIR=""
if [[ -n "$SCRIPT_SRC" && -f "$SCRIPT_SRC" ]]; then
  SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_SRC")" && pwd)"
fi
if [[ -n "$SCRIPT_DIR" && -f "${SCRIPT_DIR}/Cargo.toml" && -d "${SCRIPT_DIR}/plugin" ]]; then
  echo "→ Local source checkout detected."
  if [[ ! -x "${SCRIPT_DIR}/target/release/agy-hyper" ]]; then
    echo "→ Building release binary via cargo..."
    (cd "$SCRIPT_DIR" && cargo build --release)
  fi
  "${SCRIPT_DIR}/target/release/agy-hyper" daemon stop >/dev/null 2>&1 || true
  mkdir -p "${PLUGIN_DIR}/bin" "${HOME}/.local/bin"
  rm -rf "${PLUGIN_DIR}/sidecars" "${PLUGIN_DIR}/rules" "${PLUGIN_DIR}/assets"
  cp -R "${SCRIPT_DIR}/plugin/." "${PLUGIN_DIR}/"
  # Atomic fresh-inode replace: overwriting a running Mach-O binary in-place triggers SIGKILL on Apple Silicon.
  TMP_BIN="${PLUGIN_DIR}/bin/.agy-hyper.new.$$"
  cp "${SCRIPT_DIR}/target/release/agy-hyper" "${TMP_BIN}"
  chmod +x "${TMP_BIN}"
  mv -f "${TMP_BIN}" "${PLUGIN_DIR}/bin/agy-hyper"
  ln -sf "${PLUGIN_DIR}/bin/agy-hyper" "${BIN_LINK}"
  recycle_sidecar
  echo "✓ Installed local build to ${PLUGIN_DIR}"
  "${PLUGIN_DIR}/bin/agy-hyper" doctor
  exit 0
fi

# Remote release install with mandatory SHA-256 verification
if [[ "$VERSION" == "latest" ]]; then
  BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
  BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
fi

ARCHIVE="agy-hyper-${TARGET}.tar.gz"
echo "→ Downloading ${ARCHIVE} + SHA256SUMS..."
curl -fsSL "${BASE_URL}/${ARCHIVE}" -o "${TMP_DIR}/${ARCHIVE}"
curl -fsSL "${BASE_URL}/SHA256SUMS" -o "${TMP_DIR}/SHA256SUMS"

EXPECTED="$(grep "  ${ARCHIVE}\$" "${TMP_DIR}/SHA256SUMS" | awk '{print $1}')"
if [[ -z "$EXPECTED" ]]; then
  echo "✗ Checksum entry for ${ARCHIVE} missing in SHA256SUMS. Aborting."
  exit 1
fi

if command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
else
  ACTUAL="$(sha256sum "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
fi

if [[ "$ACTUAL" != "$EXPECTED" ]]; then
  echo "✗ SHA-256 verification FAILED!"
  echo "  expected: $EXPECTED"
  echo "  actual  : $ACTUAL"
  exit 1
fi
echo "✓ SHA-256 verified (${ACTUAL:0:16}…)"

# Atomic stage + swap (never leaves half-installed plugin on failure)
STAGE_DIR="${TMP_DIR}/stage"
mkdir -p "$STAGE_DIR"
tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$STAGE_DIR"

mkdir -p "$(dirname "$PLUGIN_DIR")" "${HOME}/.local/bin"
if [[ -x "${PLUGIN_DIR}/bin/agy-hyper" ]]; then
  "${PLUGIN_DIR}/bin/agy-hyper" daemon stop >/dev/null 2>&1 || true
fi
BACKUP_DIR="${PLUGIN_DIR}.bak.$$"
if [[ -d "$PLUGIN_DIR" ]]; then
  mv "$PLUGIN_DIR" "$BACKUP_DIR"
fi
if mv "${STAGE_DIR}/agy-hyper" "$PLUGIN_DIR"; then
  rm -rf "$BACKUP_DIR"
else
  [[ -d "$BACKUP_DIR" ]] && mv "$BACKUP_DIR" "$PLUGIN_DIR"
  echo "✗ Swap failed; previous installation restored."
  exit 1
fi

chmod +x "${PLUGIN_DIR}/bin/agy-hyper"
ln -sf "${PLUGIN_DIR}/bin/agy-hyper" "${BIN_LINK}"

recycle_sidecar

"${PLUGIN_DIR}/bin/agy-hyper" doctor
echo "⚡ Done! Open Antigravity and click AGY Hyper Deck in your side pane (or run: agy-hyper ui)."
