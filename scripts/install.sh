#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' 'Usage: bash install.sh [--version VERSION] [--bin-dir DIRECTORY]' \
    'Defaults: latest GitHub release, ~/.local/bin. Installs codex-silent only.'
}

fail() { printf 'codex-silent: %s\n' "$*" >&2; exit 1; }

release_version=latest
bin_dir=${HOME:?HOME must be set}/.local/bin
while [ "$#" -gt 0 ]; do
  case "$1" in
    --version|--bin-dir)
      [ "$#" -ge 2 ] && [ -n "$2" ] || fail "$1 requires a value"
      [[ "$2" != --* ]] || fail "$1 requires a value"
      case "$1" in
        --version) release_version=$2 ;;
        --bin-dir) bin_dir=$2 ;;
      esac
      shift 2
      ;;
    --help|-h) usage; exit 0 ;;
    *) fail "unknown argument: $1" ;;
  esac
done

if [ "$release_version" != latest ]; then
  [[ "$release_version" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]] || fail 'invalid release version'
  release_version=v${release_version#v}
fi

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) release_target=x86_64-unknown-linux-musl ;;
  Darwin/arm64|Darwin/aarch64) release_target=aarch64-apple-darwin ;;
  Darwin/x86_64) release_target=x86_64-apple-darwin ;;
  *) fail 'no prebuilt binary for this platform; use cargo install from source' ;;
esac

for dependency in curl tar awk mktemp; do
  command -v "$dependency" >/dev/null || fail "missing command: $dependency"
done
if command -v sha256sum >/dev/null; then
  checksum_command=(sha256sum)
elif command -v shasum >/dev/null; then
  checksum_command=(shasum -a 256)
else
  fail 'sha256sum or shasum is required'
fi

artifact=codex-silent-$release_target.tar.gz
release_base=https://github.com/yunweizhao26/codex-silent-mode/releases
if [ "$release_version" = latest ]; then
  download_base=$release_base/latest/download
else
  download_base=$release_base/download/$release_version
fi

download_dir=$(mktemp -d "${TMPDIR:-/tmp}/codex-silent-install.XXXXXX")
staged_binary=
cleanup() {
  if [ -n "$staged_binary" ]; then rm -f "$staged_binary"; fi
  rm -rf "$download_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

printf 'Downloading %s (%s)\n' "$artifact" "$release_version"
curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --retry 3 --connect-timeout 15 --max-time 300 \
  "$download_base/$artifact" --output "$download_dir/$artifact"
curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --retry 3 --connect-timeout 15 --max-time 300 \
  "$download_base/SHA256SUMS" --output "$download_dir/SHA256SUMS"

expected_sha=$(awk -v artifact="$artifact" '$2 == artifact { print $1; count++ } END { if (count != 1) exit 1 }' "$download_dir/SHA256SUMS") \
  || fail 'checksum manifest must contain exactly one matching artifact'
[[ "$expected_sha" =~ ^[[:xdigit:]]{64}$ ]] || fail 'invalid SHA256 checksum in manifest'
actual_sha=$("${checksum_command[@]}" < "$download_dir/$artifact" | awk '{print $1}')
[ "$actual_sha" = "$expected_sha" ] || fail 'SHA256 mismatch; nothing installed'

# Only read the expected member to stdout; never extract archive paths onto disk.
[ "$(tar -tzf "$download_dir/$artifact")" = codex-silent ] || fail 'unexpected archive contents'
tar -xOzf "$download_dir/$artifact" codex-silent > "$download_dir/codex-silent"
[ -s "$download_dir/codex-silent" ] || fail 'archive contains no binary'

mkdir -p -- "$bin_dir"
bin_dir=$(cd -- "$bin_dir" && pwd -P)
destination=$bin_dir/codex-silent
[ ! -L "$destination" ] || fail "refusing to replace symlink: $destination"
[ ! -e "$destination" ] || [ -f "$destination" ] || fail "not a regular file: $destination"
staged_binary=$(mktemp "$bin_dir/.codex-silent.XXXXXX")
cp "$download_dir/codex-silent" "$staged_binary"
chmod 755 "$staged_binary"
mv -f "$staged_binary" "$destination"
staged_binary=
printf 'Installed %s\nRun: "%s" --check\n' "$destination" "$destination"
