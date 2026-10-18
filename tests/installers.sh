#!/bin/sh
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT HUP INT TERM
MOCK="$WORK/mock"
mkdir "$MOCK"
ARTIFACT="$WORK/artifact"
INSTALL="$WORK/install"
printf 'tooler test binary\n' > "$ARTIFACT"
HASH="$(shasum -a 256 "$ARTIFACT" | awk '{print $1}')"

cat > "$MOCK/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
  -s) printf 'Linux\n' ;;
  -m) printf 'x86_64\n' ;;
esac
EOF
cat > "$MOCK/curl" <<'EOF'
#!/bin/sh
out=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    *) url="$1"; shift ;;
  esac
done
case "$url" in
  *SHA256SUMS.txt) cp "$FAKE_SUMS" "$out" ;;
  *) cp "$FAKE_ARTIFACT" "$out" ;;
esac
EOF
cat > "$MOCK/sudo" <<'EOF'
#!/bin/sh
if [ "$1" = "-v" ]; then exit 0; fi
printf '%s\n' "$*" >> "$FAKE_SUDO_LOG"
if [ "$1" = "install" ]; then cp "$4" "$FAKE_INSTALL/tooler"; fi
EOF
cat > "$MOCK/tooler" <<'EOF'
#!/bin/sh
printf 'tooler test\n'
EOF
chmod +x "$MOCK"/*

run_case() {
  name="$1"
  sums="$2"
  expected_status="$3"
  : > "$WORK/sudo.log"
  set +e
  PATH="$MOCK:$PATH" TOOLER_VERSION=vtest TOOLER_INSTALL_DIR="$INSTALL" FAKE_ARTIFACT="$ARTIFACT" FAKE_SUMS="$sums" FAKE_INSTALL="$INSTALL" FAKE_SUDO_LOG="$WORK/sudo.log" sh "$ROOT/install.sh" >"$WORK/$name.out" 2>&1
  status=$?
  set -e
  test "$status" -eq "$expected_status"
}

mkdir "$INSTALL"
printf '%s  tooler-linux-x86_64\n' "$HASH" > "$WORK/valid-sums"
run_case valid "$WORK/valid-sums" 0
cmp "$ARTIFACT" "$INSTALL/tooler"

printf '%064d  tooler-linux-x86_64\n' 0 > "$WORK/invalid-sums"
run_case invalid "$WORK/invalid-sums" 1
test ! -s "$WORK/sudo.log"

printf '%s\n' 'installer checksum tests passed'
