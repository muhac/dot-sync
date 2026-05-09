#!/bin/sh
set -eu

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
test_root="${TMPDIR:-/tmp}/dot-sync-install-tests.$$"
fake_bin="${test_root}/bin"
fixture_dir="${test_root}/fixtures"

cleanup() {
  rm -rf "$test_root"
}
trap cleanup EXIT INT TERM

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

assert_file_contains() {
  file="$1"
  text="$2"
  grep -F "$text" "$file" >/dev/null 2>&1 || {
    echo "Expected $file to contain: $text" >&2
    echo "--- $file ---" >&2
    cat "$file" >&2 || true
    fail "missing expected text"
  }
}

assert_executable() {
  file="$1"
  [ -x "$file" ] || fail "$file is not executable"
}

run_install() {
  name="$1"
  shift
  work="${test_root}/${name}"
  mkdir -p "$work/home" "$work/install"
  if ! (
    cd "$repo_root"
    PATH="${fake_bin}:$PATH" \
      HOME="${work}/home" \
      FAKE_UNAME_S="${FAKE_UNAME_S:-Linux}" \
      FAKE_UNAME_M="${FAKE_UNAME_M:-x86_64}" \
      sh ./install.sh --repo local/dot-sync --dir "${work}/install" "$@" \
      > "${work}/stdout" 2> "${work}/stderr"
  ); then
    cat "${work}/stdout" >&2 || true
    cat "${work}/stderr" >&2 || true
    fail "$name failed"
  fi
}

run_install_fails() {
  name="$1"
  shift
  work="${test_root}/${name}"
  mkdir -p "$work/home" "$work/install"
  if (
    cd "$repo_root"
    PATH="${fake_bin}:$PATH" \
      HOME="${work}/home" \
      FAKE_UNAME_S="${FAKE_UNAME_S:-Linux}" \
      FAKE_UNAME_M="${FAKE_UNAME_M:-x86_64}" \
      sh ./install.sh --repo local/dot-sync --dir "${work}/install" "$@" \
      > "${work}/stdout" 2> "${work}/stderr"
  ); then
    fail "$name unexpectedly succeeded"
  fi
}

make_archive() {
  version="$1"
  target="$2"
  package="${fixture_dir}/package-${version}-${target}"
  mkdir -p "$package"
  cat > "${package}/dot-sync" <<EOF_BIN
#!/bin/sh
echo "dot-sync ${version} ${target}"
EOF_BIN
  cat > "${package}/ds" <<EOF_BIN
#!/bin/sh
echo "Sync selected fields between structured config files"
EOF_BIN
  chmod +x "${package}/dot-sync" "${package}/ds"
  tar -C "$package" -czf "${fixture_dir}/dot-sync-${version}-${target}.tar.gz" .
}

mkdir -p "$fake_bin" "$fixture_dir"

cat > "${fake_bin}/uname" <<'EOF_UNAME'
#!/bin/sh
case "$1" in
  -s) printf '%s\n' "${FAKE_UNAME_S:-Linux}" ;;
  -m) printf '%s\n' "${FAKE_UNAME_M:-x86_64}" ;;
  *) /usr/bin/uname "$@" ;;
esac
EOF_UNAME
chmod +x "${fake_bin}/uname"

cat > "${fake_bin}/curl" <<'EOF_CURL'
#!/bin/sh
set -eu

out=""
url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      out="$2"
      shift 2
      ;;
    -*)
      shift
      ;;
    *)
      url="$1"
      shift
      ;;
  esac
done

fixture_dir="${FAKE_RELEASE_FIXTURE_DIR:?}"

if [ "$url" = "https://api.github.com/repos/local/dot-sync/releases/latest" ]; then
  content='{"tag_name":"v1.2.3"}'
elif [ "${url##*/}" = "SHA256SUMS" ]; then
  content="$(cat "${fixture_dir}/SHA256SUMS")"
else
  file="${fixture_dir}/${url##*/}"
  [ -f "$file" ] || {
    echo "missing fake asset: $url" >&2
    exit 22
  }
  if [ -n "$out" ]; then
    cp "$file" "$out"
    exit 0
  fi
  cat "$file"
  exit 0
fi

if [ -n "$out" ]; then
  printf '%s\n' "$content" > "$out"
else
  printf '%s\n' "$content"
fi
EOF_CURL
chmod +x "${fake_bin}/curl"

make_archive "v1.2.3" "x86_64-unknown-linux-gnu"
make_archive "nightly" "x86_64-unknown-linux-gnu"
make_archive "v0.1.0" "aarch64-apple-darwin"
make_archive "nightly" "x86_64-apple-darwin"

(
  cd "$fixture_dir"
  sha256sum dot-sync-*.tar.gz > SHA256SUMS
)

export FAKE_RELEASE_FIXTURE_DIR="$fixture_dir"

run_install latest_linux
assert_file_contains "${test_root}/latest_linux/stdout" "Installing dot-sync v1.2.3 for x86_64-unknown-linux-gnu"
assert_file_contains "${test_root}/latest_linux/install/dot-sync" "dot-sync v1.2.3 x86_64-unknown-linux-gnu"
assert_file_contains "${test_root}/latest_linux/install/ds" "Sync selected fields between structured config files"
assert_executable "${test_root}/latest_linux/install/dot-sync"
assert_executable "${test_root}/latest_linux/install/ds"

run_install nightly_linux --nightly
assert_file_contains "${test_root}/nightly_linux/stdout" "Installing dot-sync nightly for x86_64-unknown-linux-gnu"
assert_file_contains "${test_root}/nightly_linux/install/dot-sync" "dot-sync nightly x86_64-unknown-linux-gnu"

FAKE_UNAME_S=Darwin FAKE_UNAME_M=arm64 run_install version_macos_arm --version v0.1.0
assert_file_contains "${test_root}/version_macos_arm/stdout" "Installing dot-sync v0.1.0 for aarch64-apple-darwin"
assert_file_contains "${test_root}/version_macos_arm/install/dot-sync" "dot-sync v0.1.0 aarch64-apple-darwin"

FAKE_UNAME_S=Darwin FAKE_UNAME_M=x86_64 run_install nightly_macos_x64 --nightly
assert_file_contains "${test_root}/nightly_macos_x64/stdout" "Installing dot-sync nightly for x86_64-apple-darwin"

mkdir -p "${test_root}/ds_conflict/home" "${test_root}/ds_conflict/install"
cat > "${test_root}/ds_conflict/install/ds" <<'EOF_CONFLICT'
#!/bin/sh
echo "different ds command"
EOF_CONFLICT
chmod +x "${test_root}/ds_conflict/install/ds"
(
  cd "$repo_root"
  PATH="${fake_bin}:$PATH" \
    HOME="${test_root}/ds_conflict/home" \
    FAKE_UNAME_S=Linux \
    FAKE_UNAME_M=x86_64 \
    sh ./install.sh --repo local/dot-sync --dir "${test_root}/ds_conflict/install" --nightly \
    > "${test_root}/ds_conflict/stdout" 2> "${test_root}/ds_conflict/stderr"
)
assert_file_contains "${test_root}/ds_conflict/install/dot-sync" "dot-sync nightly x86_64-unknown-linux-gnu"
assert_file_contains "${test_root}/ds_conflict/install/ds" "different ds command"
assert_file_contains "${test_root}/ds_conflict/stderr" "already exists and does not look like dot-sync"

run_install_fails mutually_exclusive --nightly --version v0.1.0
assert_file_contains "${test_root}/mutually_exclusive/stderr" "mutually exclusive"

FAKE_UNAME_S=Linux FAKE_UNAME_M=arm64 run_install_fails unsupported_linux_arch --nightly
assert_file_contains "${test_root}/unsupported_linux_arch/stderr" "unsupported Linux architecture"

cp "${fixture_dir}/SHA256SUMS" "${fixture_dir}/SHA256SUMS.good"
grep -v 'dot-sync-nightly-x86_64-unknown-linux-gnu.tar.gz' "${fixture_dir}/SHA256SUMS.good" > "${fixture_dir}/SHA256SUMS"
FAKE_UNAME_S=Linux FAKE_UNAME_M=x86_64 run_install_fails missing_checksum --nightly
assert_file_contains "${test_root}/missing_checksum/stderr" "checksum for dot-sync-nightly-x86_64-unknown-linux-gnu.tar.gz not found"
mv "${fixture_dir}/SHA256SUMS.good" "${fixture_dir}/SHA256SUMS"

echo "installer tests passed"
