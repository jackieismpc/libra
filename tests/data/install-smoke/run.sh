#!/bin/sh
# install.sh smoke harness — seven scenario families (plan-20260821 A1-05).
#
#   bash tests/data/install-smoke/run.sh
#
# Contract (ADR-UP01-03/06): the harness copies the production installer to a
# temp dir and rewrites ONLY the clearly-marked trust/origin constants in the
# COPY (test public key, local fixture server); the production file must stay
# byte-identical and expose zero runtime key-override entry points. Each
# scenario asserts the exit code AND whether a binary landed on disk.
#
# Requires: python3 (fixture HTTP server + marker rewrite), openssl >= 1.1.1
# with Ed25519 (the verifier under test), sha256sum/shasum.

set -eu

SMOKE_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SMOKE_DIR/../../.." && pwd)
INSTALLER="$REPO_ROOT/install.sh"
FIXTURES="$SMOKE_DIR/fixtures"

WORK=$(mktemp -d)
SERVER_PID=""
cleanup() {
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $1" >&2
    [ -f "$WORK/out.log" ] && sed 's/^/  | /' "$WORK/out.log" >&2
    exit 1
}

# ── global production-file assertions ───────────────────────────────────────

cp "$INSTALLER" "$WORK/install.sh.orig"

# Exactly one fixed-literal definition of each trust constant; no environment
# indirection anywhere near them (zero runtime key-override entry points).
grep -c '^LIBRA_RELEASE_MANIFEST_PUBLIC_KEY_HEX="[0-9a-f]\{64\}"$' "$INSTALLER" \
    | grep -qx '1' || fail "public-key hex constant is not a single fixed literal"
grep -q 'LIBRA_INSTALL_PUBLIC_KEY' "$INSTALLER" \
    && fail "found a runtime public-key override entry point" || true
grep -q 'LIBRA_RELEASE_MANIFEST_PUBLIC_KEY[^=]*:-' "$INSTALLER" \
    && fail "trust constants must not read the environment" || true
grep -q 'LIBRA_RELEASE_MANIFEST_ORIGIN[^=]*:-' "$INSTALLER" \
    && fail "pinned origin must not read the environment" || true

# ── fixture server ──────────────────────────────────────────────────────────

DOCROOT="$WORK/docroot"
mkdir -p "$DOCROOT"
cp -R "$FIXTURES/tree/." "$DOCROOT/"

python3 - "$DOCROOT" "$WORK/port" > "$WORK/server.log" 2>&1 <<'EOF' &
import http.server, socketserver, sys, os
os.chdir(sys.argv[1])
handler = http.server.SimpleHTTPRequestHandler
handler.log_message = lambda *a, **k: None
socketserver.TCPServer.allow_reuse_address = True
httpd = socketserver.TCPServer(("127.0.0.1", 0), handler)
with open(sys.argv[2], "w") as f:
    f.write(str(httpd.server_address[1]))
httpd.serve_forever()
EOF
SERVER_PID=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -s "$WORK/port" ] && break
    sleep 0.3
done
[ -s "$WORK/port" ] || fail "fixture server did not report a port"
PORT=$(cat "$WORK/port")
BASE="http://127.0.0.1:$PORT"
curl -fsS -o /dev/null "$BASE/libra/releases/v9.9.9/libra-linux-amd64" \
    || fail "fixture server did not come up on $BASE"

# ── prepared installer copy ─────────────────────────────────────────────────

python3 - "$INSTALLER" "$WORK/install-copy.sh" "$BASE" <<'EOF'
import re, sys
src_path, dst_path, base = sys.argv[1:4]
src = open(src_path).read()

PROD_PEM = """-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAaKoA6pNY1FVkUBDYEdQHArP2fOxL3/UtPU+4EHr67tM=
-----END PUBLIC KEY-----"""
TEST_PEM = """-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAqKAN7RPdr6rVJfq93BPvxxeynr7VDNbWUxlgV/qPikM=
-----END PUBLIC KEY-----"""

def replace(pattern, repl, count_expected=1, regex=False):
    global src
    if regex:
        src, n = re.subn(pattern, repl, src)
    else:
        n = src.count(pattern)
        src = src.replace(pattern, repl)
    if n != count_expected:
        raise SystemExit(f"marker drift: {pattern!r} matched {n} times, expected {count_expected}")

replace(PROD_PEM, TEST_PEM)
replace('LIBRA_RELEASE_MANIFEST_KEY_ID="libra-release-1"',
        'LIBRA_RELEASE_MANIFEST_KEY_ID="libra-release-test-1"')
replace('LIBRA_RELEASE_MANIFEST_PUBLIC_KEY_HEX="68aa00ea9358d455645010d811d40702b3f67cec4bdff52d3d4fb8107afaeed3"',
        'LIBRA_RELEASE_MANIFEST_PUBLIC_KEY_HEX="a8a00ded13ddafaad525fabddc13efc717b29ebed50cd6d653196057fa8f8a43"')
replace('LIBRA_RELEASE_MANIFEST_ORIGIN="https://download.libra.tools"',
        f'LIBRA_RELEASE_MANIFEST_ORIGIN="{base}"')
replace('BASE_URL="${LIBRA_BASE_URL:-https://download.libra.tools/libra/releases}"',
        'BASE_URL="${LIBRA_BASE_URL:-' + base + '/libra/releases}"')
replace(r'DEFAULT_VERSION="v[0-9][0-9.]*"', 'DEFAULT_VERSION="v9.9.8"', regex=True)
replace('api_url="https://api.github.com/repos/libra-tools/libra/releases/latest"',
        f'api_url="{base}/repos/libra-tools/libra/releases/latest"')
# NOTE: the signed-URL origin pin and the `${STABLE_URL#...}` fetch rewrite
# stay untouched: signed URLs are production-form by contract, and the copy
# re-bases only the FETCH through the rewritten origin constant.

open(dst_path, "w").write(src)
EOF
chmod +x "$WORK/install-copy.sh"

# A stub PATH dir whose openssl always fails: the verifier-unavailable state.
mkdir -p "$WORK/no-openssl"
printf '#!/bin/sh\nexit 1\n' > "$WORK/no-openssl/openssl"
chmod +x "$WORK/no-openssl/openssl"

# ── scenario runner ─────────────────────────────────────────────────────────

SCENARIOS_RUN=0

# run_scenario <name> <manifest|-none-> <expect: ok|fail> <expect-installed: yes|no> \
#              <required-output-substring> [extra env assignments...]
run_scenario() {
    name=$1; manifest=$2; expect=$3; expect_installed=$4; needle=$5
    shift 5

    rm -f "$DOCROOT/libra/releases/stable/manifest-v1.json"
    if [ "$manifest" != "-none-" ]; then
        mkdir -p "$DOCROOT/libra/releases/stable"
        cp "$FIXTURES/$manifest" "$DOCROOT/libra/releases/stable/manifest-v1.json"
    fi

    home="$WORK/home-$name"
    mkdir -p "$home"
    rc=0
    env -i PATH="${SMOKE_PATH:-$PATH}" HOME="$home" TMPDIR="$WORK" \
        LIBRA_NO_TUI=1 NO_COLOR=1 LIBRA_HOME="$home/.libra" "$@" \
        sh "$WORK/install-copy.sh" > "$WORK/out.log" 2>&1 < /dev/null || rc=$?

    if [ "$expect" = "ok" ] && [ "$rc" -ne 0 ]; then
        fail "$name: expected success, exited $rc"
    fi
    if [ "$expect" = "fail" ] && [ "$rc" -eq 0 ]; then
        fail "$name: expected failure, exited 0"
    fi
    if [ "$expect_installed" = "yes" ] && [ ! -x "$home/.libra/bin/libra" ]; then
        fail "$name: expected an installed binary at \$LIBRA_HOME/bin/libra"
    fi
    if [ "$expect_installed" = "no" ] && [ -e "$home/.libra/bin/libra" ]; then
        fail "$name: a binary was installed on a fail-closed path"
    fi
    grep -qi "$needle" "$WORK/out.log" \
        || fail "$name: output does not mention '$needle'"
    SCENARIOS_RUN=$((SCENARIOS_RUN + 1))
    echo "ok: $name"
}

# Host platform (must be in the release matrix for the harness to run).
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)            HOST_PLATFORM=linux-amd64 ;;
    Linux-aarch64|Linux-arm64) HOST_PLATFORM=linux-arm64 ;;
    Darwin-arm64)            HOST_PLATFORM=darwin-arm64 ;;
    *) fail "unsupported harness platform $(uname -s)-$(uname -m)" ;;
esac

# 1. Valid signed manifest → verified install.
run_scenario valid manifest-valid.json ok yes "stable manifest verified"
cmp -s "$WORK/home-valid/.libra/bin/libra" \
    "$FIXTURES/tree/libra/releases/v9.9.9/libra-$HOST_PLATFORM" \
    || fail "valid: installed binary differs from the signed artifact"

# 2. Tampered signature → fail closed, nothing on disk.
run_scenario bad-signature manifest-bad-signature.json fail no "SIGNATURE VERIFICATION FAILED"

# 3. Signed sha256 does not match the artifact → fail closed.
run_scenario sha-mismatch manifest-sha-mismatch.json fail no "sha256 mismatch against the SIGNED manifest"

# 4. Signed size does not match the artifact → fail closed.
run_scenario size-mismatch manifest-size-mismatch.json fail no "size mismatch"

# 5. Manifest 404 (chain not enabled) without opt-in → explicit stop.
run_scenario transition-404 -none- fail no "signature chain is not enabled yet"

# 6. Manifest 404 + LIBRA_ALLOW_FALLBACK=1 → explicit UNVERIFIED legacy install.
run_scenario transition-404-fallback -none- ok yes "proceeding UNVERIFIED" \
    LIBRA_ALLOW_FALLBACK=1
cmp -s "$WORK/home-transition-404-fallback/.libra/bin/libra" \
    "$FIXTURES/tree/libra/releases/v9.9.8/libra-$HOST_PLATFORM" \
    || fail "transition-404-fallback: legacy binary content mismatch"

# 7. Verifier unavailable without opt-in → explicit stop (third state).
SMOKE_PATH="$WORK/no-openssl:$PATH"
run_scenario verifier-unavailable manifest-valid.json fail no "signature verifier unavailable"

# 8. Verifier unavailable + LIBRA_ALLOW_FALLBACK=1 → explicit UNVERIFIED install.
run_scenario verifier-unavailable-fallback manifest-valid.json ok yes "proceeding UNVERIFIED" \
    LIBRA_ALLOW_FALLBACK=1
SMOKE_PATH=""

# ── production file untouched ───────────────────────────────────────────────
cmp -s "$INSTALLER" "$WORK/install.sh.orig" \
    || fail "the production install.sh was modified by the harness"

[ "$SCENARIOS_RUN" -eq 8 ] || fail "expected 8 scenarios, ran $SCENARIOS_RUN"
echo "install-smoke: all $SCENARIOS_RUN scenarios passed"
