#!/usr/bin/env bash
# Builds the vendored zai-proxy sidecar binary for embedding into zagent.
# Usage: scripts/build-zai-proxy-sidecar.sh [target-triple ...]
#        (default: host target; outputs crates/zai_proxy_sidecar/binaries/)
#
# Credentials are injected at link time via -ldflags -X. Values come from the
# environment or scripts/.zai-proxy-secrets.env (gitignored; locally) and from
# GitHub Actions secrets (CI). Without them the binaries embed REDACTED
# placeholders and q-bless cannot decrypt DeviceConfig ("bad PKCS7 padding") —
# see the credential var block in vendor/zai-proxy/cmd/q-bless/main.go.
set -euo pipefail
cd "$(dirname "$0")/.."

# Load a ZAI_* credential from the environment, falling back to the local
# (gitignored) secrets file. Environment always wins.
load_secret() {
  local name="$1"
  if [ -n "${!name:-}" ]; then return; fi
  if [ ! -f scripts/.zai-proxy-secrets.env ]; then return; fi
  local value
  value="$(grep -E "^${name}=" scripts/.zai-proxy-secrets.env | tail -1 | cut -d= -f2-)"
  if [ -n "$value" ]; then export "${name}=${value}"; fi
}

load_secret ZAI_KEY_WRAP
load_secret ZAI_KEY_WDC
load_secret ZAI_DUANE_ID
load_secret ZAI_DUANE_SECRET
load_secret ZAI_BRIDGE_ACCESS_KEY
load_secret ZAI_BRIDGE_SECRET_KEY
load_secret ZAI_ALIYUN_ACCESS_KEY
load_secret ZAI_ALIYUN_SECRET_KEY

PKG_QB="main"
PKG_ZB="zai-api/internal/zbridge"
LDFLAGS="-s -w"
append_x() { # append_x <go-var-path> <env-name>
  local value="${2:-}"
  if [ -n "$value" ]; then LDFLAGS="$LDFLAGS -X $1=$value"; fi
}
append_x "${PKG_QB}.keyWWrap"               "${ZAI_KEY_WRAP:-}"
append_x "${PKG_QB}.keyWDC"                 "${ZAI_KEY_WDC:-}"
append_x "${PKG_QB}.duaneID"                "${ZAI_DUANE_ID:-}"
append_x "${PKG_QB}.duaneSecret"            "${ZAI_DUANE_SECRET:-}"
append_x "${PKG_QB}.bridgeAccessKey"        "${ZAI_BRIDGE_ACCESS_KEY:-}"
append_x "${PKG_QB}.bridgeSecretKey"        "${ZAI_BRIDGE_SECRET_KEY:-}"
append_x "${PKG_ZB}.defaultAliyunAccessKey" "${ZAI_ALIYUN_ACCESS_KEY:-}"
append_x "${PKG_ZB}.defaultAliyunSecretKey" "${ZAI_ALIYUN_SECRET_KEY:-}"

case "$LDFLAGS" in
  *"-X "*) echo "[✓] credential injection active" ;;
  *) echo "[!] WARNING: no ZAI_* credentials set — building with REDACTED placeholders" >&2
     echo "    q-bless will fail with 'bad PKCS7 padding'; set scripts/.zai-proxy-secrets.env" >&2 ;;
esac

TARGETS=("$@")
if [ ${#TARGETS[@]} -eq 0 ]; then
  TARGETS=("$(rustc -vV | awk '/host:/ {print $2}')")
fi

for triple in "${TARGETS[@]}"; do
  case "$triple" in
    *linux*)  goos=linux ;;
    *darwin*) goos=darwin ;;
    *windows*) goos=windows ;;
    *) echo "unsupported triple: $triple" >&2; exit 1 ;;
  esac
  case "$triple" in
    *aarch64*|*arm64*) goarch=arm64 ;;
    *)                 goarch=amd64 ;;
  esac
  out="crates/zai_proxy_sidecar/binaries/zai-proxy-${goos}-${goarch}"
  echo "[*] building $out (GOOS=$goos GOARCH=$goarch)"
  (cd vendor/zai-proxy && GOOS=$goos GOARCH=$goarch CGO_ENABLED=0 \
       go build -trimpath -mod=mod -buildvcs=false -ldflags "$LDFLAGS" -o "../../$out" .)
  qout="crates/zai_proxy_sidecar/binaries/q-bless-${goos}-${goarch}"
  echo "[*] building $qout"
  (cd vendor/zai-proxy && GOOS=$goos GOARCH=$goarch CGO_ENABLED=0 \
       go build -trimpath -mod=mod -buildvcs=false -ldflags "$LDFLAGS" -o "../../$qout" ./cmd/q-bless)
 done
echo "[✓] sidecar binaries ready"
