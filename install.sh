#!/bin/sh
set -eu

repo="muhac/dot-sync"
version="latest"
install_dir="${HOME}/.local/bin"

usage() {
  cat <<'USAGE'
Install dot-sync from GitHub releases.

Usage:
  install.sh [--nightly | --version <version>] [--dir <path>] [--repo <owner/name>]

Options:
  --nightly           Install the nightly prerelease.
  --version <version> Install a specific release tag, such as v0.1.0.
  --dir <path>        Install directory. Defaults to ~/.local/bin.
  --repo <owner/name> Override the GitHub repository. Defaults to muhac/dot-sync.
  -h, --help          Show this help.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --nightly)
      if [ "$version" != "latest" ]; then
        echo "error: --nightly and --version are mutually exclusive" >&2
        exit 1
      fi
      version="nightly"
      shift
      ;;
    --version)
      if [ "$version" != "latest" ]; then
        echo "error: --nightly and --version are mutually exclusive" >&2
        exit 1
      fi
      if [ "$#" -lt 2 ]; then
        echo "error: --version requires a value" >&2
        exit 1
      fi
      version="$2"
      shift 2
      ;;
    --dir)
      if [ "$#" -lt 2 ]; then
        echo "error: --dir requires a value" >&2
        exit 1
      fi
      install_dir="$2"
      shift 2
      ;;
    --repo)
      if [ "$#" -lt 2 ]; then
        echo "error: --repo requires a value" >&2
        exit 1
      fi
      repo="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown option: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: required command not found: $1" >&2
    exit 1
  fi
}

need curl
need tar
need uname

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64|amd64) target="x86_64-apple-darwin" ;;
      *)
        echo "error: unsupported macOS architecture: $arch" >&2
        exit 1
        ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      *)
        echo "error: unsupported Linux architecture: $arch" >&2
        exit 1
        ;;
    esac
    ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "error: Windows shell install is not supported yet; download the zip from GitHub releases" >&2
    exit 1
    ;;
  *)
    echo "error: unsupported operating system: $os" >&2
    exit 1
    ;;
esac

if [ "$version" = "latest" ]; then
  need sed
  latest_url="https://api.github.com/repos/${repo}/releases/latest"
  version="$(
    curl -fsSL "$latest_url" |
      sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
      sed -n '1p'
  )"
  if [ -z "$version" ]; then
    echo "error: failed to resolve latest release for $repo" >&2
    exit 1
  fi
fi

archive="dot-sync-${version}-${target}.tar.gz"
base_url="https://github.com/${repo}/releases/download/${version}"
tmp_dir="$(mktemp -d)"

cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT INT TERM

echo "Installing dot-sync ${version} for ${target}"

curl -fsSL "${base_url}/${archive}" -o "${tmp_dir}/${archive}"
curl -fsSL "${base_url}/SHA256SUMS" -o "${tmp_dir}/SHA256SUMS"

if command -v sha256sum >/dev/null 2>&1; then
  checksum_cmd="sha256sum -c"
elif command -v shasum >/dev/null 2>&1; then
  checksum_cmd="shasum -a 256 -c"
else
  checksum_cmd=""
fi

if [ -n "$checksum_cmd" ]; then
  if ! grep " ${archive}\$" "${tmp_dir}/SHA256SUMS" > "${tmp_dir}/${archive}.sha256"; then
    echo "error: checksum for ${archive} not found in SHA256SUMS" >&2
    exit 1
  fi
  (cd "$tmp_dir" && $checksum_cmd "${archive}.sha256")
else
  echo "warning: sha256sum/shasum not found; skipping checksum verification" >&2
fi

tar -xzf "${tmp_dir}/${archive}" -C "$tmp_dir"
mkdir -p "$install_dir"
install -m 755 "${tmp_dir}/dot-sync" "${install_dir}/dot-sync"

echo "Installed dot-sync to ${install_dir}/dot-sync"

if [ -e "${install_dir}/ds" ]; then
  if "${install_dir}/ds" --help 2>/dev/null | grep -q "Sync selected fields between structured config files"; then
    install -m 755 "${tmp_dir}/ds" "${install_dir}/ds"
    echo "Updated ds to ${install_dir}/ds"
  else
    echo "warning: ${install_dir}/ds already exists and does not look like dot-sync; leaving it unchanged" >&2
    echo "warning: use dot-sync directly, or remove ${install_dir}/ds and rerun this installer to install the ds alias" >&2
  fi
else
  install -m 755 "${tmp_dir}/ds" "${install_dir}/ds"
  echo "Installed ds to ${install_dir}/ds"
fi

case ":$PATH:" in
  *":${install_dir}:"*) ;;
  *) echo "note: ${install_dir} is not on PATH" >&2 ;;
esac
