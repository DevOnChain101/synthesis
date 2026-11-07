#!/usr/bin/env bash
# CI runtime smoke of the GPUI desktop app (docs/specs/desktop.md "CI"). Usage: desktop-smoke.sh OUT_DIR
# macOS/Linux: offscreen renders (--shot --check: size, not blank). Windows has no headless renderer in
# gpui-pre-windows, so a real window must draw its first frame.
set -euo pipefail
out=${1:?out dir}
mkdir -p "$out"
cargo build -p synthesis-desktop --locked --features shot,test-provider
bin=target/debug/synthesis-desktop
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    bin=$bin.exe
    # WARP (software D3D11) is used by Windows when the runner has no GPU
    "$bin" --sample --exit-after-first-frame >"$out/first-frame.log" 2>&1 &
    pid=$!
    for _ in $(seq 60); do kill -0 $pid 2>/dev/null || break; sleep 1; done
    kill -0 $pid 2>/dev/null && { echo "no first frame within 60 s"; taskkill //F //IM synthesis-desktop.exe || true; exit 1; }
    wait $pid
    cat "$out/first-frame.log"
    grep -q "first frame drawn" "$out/first-frame.log"
    ;;
  *)
    "$bin" --shot "$out/tui-1400x900.png" --size 1400x900 --check
    "$bin" --shot "$out/tui-1000x700.png" --size 1000x700 --check
    "$bin" --shot "$out/settings-1400x900.png" --size 1400x900 --view settings --check
    ;;
esac
