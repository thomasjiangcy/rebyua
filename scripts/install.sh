#!/bin/sh

set -eu

REPOSITORY="${RBA_RELEASE_REPOSITORY:-thomasjiangcy/rebyua}"
INSTALL_DIR="${RBA_INSTALL_DIR:-/usr/local/bin}"

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 1
  }
}

need_cmd uname
need_cmd mktemp
need_cmd tar
need_cmd find
need_cmd install
need_cmd sed
need_cmd head

if command -v curl >/dev/null 2>&1; then
  fetch() {
    curl -fsSL "$1"
  }
elif command -v wget >/dev/null 2>&1; then
  fetch() {
    wget -qO- "$1"
  }
else
  echo "missing required command: curl or wget" >&2
  exit 1
fi

platform() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "${os}:${arch}" in
    Darwin:arm64)
      echo "macos-aarch64"
      ;;
    Darwin:x86_64)
      echo "macos-x86_64"
      ;;
    Linux:x86_64)
      echo "linux-x86_64"
      ;;
    *)
      echo "unsupported platform: ${os}-${arch}" >&2
      exit 1
      ;;
  esac
}

latest_tag() {
  api_url="https://api.github.com/repos/${REPOSITORY}/releases/latest"
  fetch "${api_url}" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1
}

resolve_install_dir() {
  if [ -w "${INSTALL_DIR}" ] || [ ! -e "${INSTALL_DIR}" ] && mkdir -p "${INSTALL_DIR}" 2>/dev/null; then
    echo "${INSTALL_DIR}"
    return
  fi

  fallback="${HOME}/.local/bin"
  mkdir -p "${fallback}"
  echo "${fallback}"
}

TAG="$(latest_tag)"
[ -n "${TAG}" ] || {
  echo "failed to resolve latest release tag" >&2
  exit 1
}

PLATFORM="$(platform)"
ARCHIVE="reb-${TAG}-${PLATFORM}.tar.gz"
URL="https://github.com/${REPOSITORY}/releases/download/${TAG}/${ARCHIVE}"
TARGET_DIR="$(resolve_install_dir)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT INT TERM

echo "Downloading ${URL}"
fetch "${URL}" > "${TMP_DIR}/${ARCHIVE}"
tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "${TMP_DIR}"

BINARY_PATH="$(find "${TMP_DIR}" -type f -name reb | head -n 1)"
[ -n "${BINARY_PATH}" ] || {
  echo "failed to locate reb in downloaded archive" >&2
  exit 1
}

install -m 0755 "${BINARY_PATH}" "${TARGET_DIR}/reb"

echo "Installed reb to ${TARGET_DIR}/reb"
case ":${PATH}:" in
  *:"${TARGET_DIR}":*)
    ;;
  *)
    echo "Note: ${TARGET_DIR} is not on PATH" >&2
    ;;
esac
