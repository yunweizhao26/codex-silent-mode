#!/usr/bin/env bash
# Local fixtures only. Never downloads or executes a release binary.
set -euo pipefail
repo_dir=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/codex-silent-installer-test.XXXXXX")
trap 'rm -rf "$fixture_dir"' EXIT
mkdir -p "$fixture_dir/mock" "$fixture_dir/source" "$fixture_dir/releases" "$fixture_dir/bin"
printf 'fixture binary; do not execute\n' > "$fixture_dir/source/codex-silent"
printf 'existing codex\n' > "$fixture_dir/bin/codex"

cat > "$fixture_dir/mock/uname" <<'MOCK'
#!/usr/bin/env bash
case "$1" in
  -s) printf '%s\n' "${MOCK_SYSTEM:-Linux}" ;;
  -m) printf '%s\n' "${MOCK_ARCH:-x86_64}" ;;
  *) exit 1 ;;
esac
MOCK
cat > "$fixture_dir/mock/curl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
download_url= output_file=
while [ "$#" -gt 0 ]; do
  case "$1" in
    https://*) download_url=$1; shift ;;
    --output) output_file=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s\n' "$download_url" >> "$FIXTURE_DIR/requests"
cp "$FIXTURE_DIR/releases/${download_url##*/}" "$output_file"
MOCK
chmod +x "$fixture_dir/mock/uname" "$fixture_dir/mock/curl"
export FIXTURE_DIR=$fixture_dir
export PATH="$fixture_dir/mock:$PATH"

artifact=codex-silent-x86_64-unknown-linux-musl.tar.gz
COPYFILE_DISABLE=1 tar -czf "$fixture_dir/releases/$artifact" -C "$fixture_dir/source" codex-silent
if command -v sha256sum >/dev/null; then
  fixture_sha=$(sha256sum < "$fixture_dir/releases/$artifact" | awk '{print $1}')
else
  fixture_sha=$(shasum -a 256 < "$fixture_dir/releases/$artifact" | awk '{print $1}')
fi
printf '%s  %s\n' "$fixture_sha" "$artifact" > "$fixture_dir/releases/SHA256SUMS"

run_installer() { bash "$repo_dir/scripts/install.sh" "$@"; }
expect_failure() {
  if run_installer "$@" > "$fixture_dir/failure.log" 2>&1; then
    printf 'Unexpected installation success: %s\n' "$*" >&2
    exit 1
  fi
}

run_installer --version 0.2.0 --bin-dir "$fixture_dir/bin"
cmp "$fixture_dir/source/codex-silent" "$fixture_dir/bin/codex-silent"
[ -x "$fixture_dir/bin/codex-silent" ]
[ "$(cat "$fixture_dir/bin/codex")" = 'existing codex' ]
grep -q '/download/v0.2.0/' "$fixture_dir/requests"

run_installer --bin-dir "$fixture_dir/path with spaces"
cmp "$fixture_dir/source/codex-silent" "$fixture_dir/path with spaces/codex-silent"
grep -q '/latest/download/' "$fixture_dir/requests"

for mac_target in aarch64-apple-darwin x86_64-apple-darwin; do
  mac_artifact=codex-silent-$mac_target.tar.gz
  cp "$fixture_dir/releases/$artifact" "$fixture_dir/releases/$mac_artifact"
  printf '%s  %s\n' "$fixture_sha" "$mac_artifact" > "$fixture_dir/releases/SHA256SUMS"
  export MOCK_SYSTEM=Darwin
  if [ "$mac_target" = aarch64-apple-darwin ]; then export MOCK_ARCH=arm64; else export MOCK_ARCH=x86_64; fi
  run_installer --version v0.2.0 --bin-dir "$fixture_dir/$mac_target"
  cmp "$fixture_dir/source/codex-silent" "$fixture_dir/$mac_target/codex-silent"
done
export MOCK_SYSTEM=Linux MOCK_ARCH=x86_64
printf '%s  %s\n' "$fixture_sha" "$artifact" > "$fixture_dir/releases/SHA256SUMS"

expect_failure --version '../invalid' --bin-dir "$fixture_dir/bin"
expect_failure --bin-dir
expect_failure --bin-dir --version v0.2.0
mkdir "$fixture_dir/symlink-bin"
ln -s "$fixture_dir/bin/codex" "$fixture_dir/symlink-bin/codex-silent"
expect_failure --bin-dir "$fixture_dir/symlink-bin"
[ "$(cat "$fixture_dir/bin/codex")" = 'existing codex' ]

mkdir "$fixture_dir/hardlink-bin"
ln "$fixture_dir/bin/codex" "$fixture_dir/hardlink-bin/codex-silent"
run_installer --bin-dir "$fixture_dir/hardlink-bin"
[ "$(cat "$fixture_dir/bin/codex")" = 'existing codex' ]

printf '%064d  %s\n' 0 "$artifact" > "$fixture_dir/releases/SHA256SUMS"
expect_failure --bin-dir "$fixture_dir/bin"
cmp "$fixture_dir/source/codex-silent" "$fixture_dir/bin/codex-silent"
expect_failure --bin-dir "$fixture_dir/uncreated"
[ ! -e "$fixture_dir/uncreated" ]

printf '%s  %s\n%s  %s\n' "$fixture_sha" "$artifact" "$fixture_sha" "$artifact" > "$fixture_dir/releases/SHA256SUMS"
expect_failure --bin-dir "$fixture_dir/bin"
printf '%s  unrelated.tar.gz\n' "$fixture_sha" > "$fixture_dir/releases/SHA256SUMS"
expect_failure --bin-dir "$fixture_dir/bin"
export MOCK_SYSTEM=Linux MOCK_ARCH=aarch64
expect_failure --bin-dir "$fixture_dir/bin"
printf 'Installer fixture checks passed.\n'
