#!/usr/bin/env bash
# tools/linux_test.sh - set up a Linux machine for kode-sim and test it, stage by stage.
#
#   tools/linux_test.sh                      # everything, on this checkout; asks before sudo
#   tools/linux_test.sh --stage boot,emu     # some stages
#   tools/linux_test.sh --no-wifi --stage boot   # without the C5 (no firmware build)
#
# Run from a checkout, it tests that checkout (your fork, your branch). Run on its
# own (curl ... | bash, or a copy), it clones --repo (default upstream) into $WORK.
#
# Stages, in order:
#   deps    distro packages, Rust (rustup), ESP-IDF v6.1 for esp32p4+esp32c5
#   build   clone kode-sim, cargo build --release, link ~/.local/bin/kode-sim
#   host    what upstream CI runs: rustfmt, clippy, tools/host_tests.sh
#   boot    the KodeOS factory release, headless in esp-emu, with the C5 (WiFi)
#   emu     the emulator suite: pytest -m emu tests (slow: 20-60 min on a laptop)
#   window  kode-sim run kode-app-template in its window (interactive, needs a desktop)
#
# Everything lands in a report folder ($WORK/reports/<time>/) with summary.md at the
# top: the machine, each stage's verdict, and the logs - attach it to a bug ticket.

set -uo pipefail

REPO="https://github.com/kodediy/kode-sim.git"
BRANCH=""
WORK="$HOME/kodedot"
STAGES="deps build host boot emu window"
ASSUME_YES=0
NO_WIFI=0
BOOT_SECS=240

usage() { sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO="$2"; shift 2 ;;
    --branch) BRANCH="$2"; shift 2 ;;
    --dir) WORK="$2"; shift 2 ;;
    --stage) STAGES="${2//,/ }"; shift 2 ;;
    --skip) for s in ${2//,/ }; do STAGES="${STAGES//$s/}"; done; shift 2 ;;
    --no-wifi) NO_WIFI=1; shift ;;
    --boot-secs) BOOT_SECS="$2"; shift 2 ;;
    -y|--yes) ASSUME_YES=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown option: $1" >&2; usage 2 ;;
  esac
done

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$HERE/../Cargo.toml" ] && grep -q 'crates/kode-sim' "$HERE/../Cargo.toml" 2>/dev/null; then
  SIM="$(cd "$HERE/.." && pwd)"; IN_TREE=1
else
  SIM="$WORK/kode-sim"; IN_TREE=0
fi
KS="$SIM/target/release/kode-sim"
IDF_DIR="$WORK/esp-idf-v6.1"
REPORT="$WORK/reports/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$REPORT"
SUMMARY="$REPORT/summary.md"
RESULTS=()

c_ok=$'\e[32m'; c_bad=$'\e[31m'; c_dim=$'\e[2m'; c_off=$'\e[0m'
say()  { printf '%s==>%s %s\n' "$c_dim" "$c_off" "$*"; }
pass() { RESULTS+=("| $1 | PASS | ${2:-} |"); printf '%sPASS%s %s %s\n' "$c_ok" "$c_off" "$1" "${2:-}"; }
fail() { RESULTS+=("| $1 | **FAIL** | ${2:-} |"); printf '%sFAIL%s %s %s\n' "$c_bad" "$c_off" "$1" "${2:-}"; }
warn() { RESULTS+=("| $1 | warn | ${2:-} |"); printf '%sWARN%s %s %s\n' "$c_bad" "$c_off" "$1" "${2:-}"; }
have() { command -v "$1" >/dev/null 2>&1; }
want() { case " $STAGES " in *" $1 "*) return 0 ;; esac; return 1; }
confirm() {
  [ "$ASSUME_YES" = 1 ] && return 0
  read -r -p "$1 [y/N] " a; [[ "$a" =~ ^[Yy] ]]
}

# ---------------------------------------------------------------- the machine
machine_info() {
  {
    echo "# kode-sim on Linux - test report"
    echo
    echo "- date: $(date -Is)"
    echo "- kernel: $(uname -srm)"
    [ -r /etc/os-release ] && echo "- distro: $(. /etc/os-release; echo "$PRETTY_NAME")"
    echo "- cpu: $(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | xargs) ($(nproc) threads)"
    echo "- memory: $(awk '/MemTotal/ {printf "%.1f GB", $2/1048576}' /proc/meminfo)"
    echo "- session: ${XDG_SESSION_TYPE:-none} ${XDG_CURRENT_DESKTOP:-}"
    echo "- audio: $(have pactl && pactl info 2>/dev/null | grep -m1 'Server Name' | cut -d: -f2- | xargs || echo unknown)"
    [ "$IN_TREE" = 1 ] && echo "- repo: $(git -C "$SIM" remote get-url origin 2>/dev/null) ($(git -C "$SIM" branch --show-current))" || echo "- repo: $REPO ${BRANCH:+(branch $BRANCH)}"
    [ -e /sys/class/net/tap0 ] && echo "- **tap0 exists**: the P4's esp-emu will grab it (kode-sim passes no --net to the P4)"
  } > "$REPORT/machine.md"
}

# ---------------------------------------------------------------- deps
pkg_install() {
  local pm="" pkgs=""
  if have apt-get; then
    pm="sudo apt-get install -y"
    pkgs="git curl wget unzip build-essential cmake ninja-build ccache flex bison gperf \
          python3 python3-pip python3-venv libffi-dev libssl-dev dfu-util libusb-1.0-0 \
          mtools nodejs pkg-config"
    sudo_prefix="sudo apt-get update -qq &&"
  elif have dnf; then
    pm="sudo dnf install -y"
    pkgs="git curl wget unzip gcc gcc-c++ make cmake ninja-build ccache flex bison gperf \
          python3 python3-pip libffi-devel openssl-devel dfu-util libusbx mtools nodejs"
    sudo_prefix=""
  elif have pacman; then
    pm="sudo pacman -S --needed --noconfirm"
    pkgs="git curl wget unzip base-devel cmake ninja ccache flex bison gperf python python-pip \
          dfu-util libusb mtools nodejs"
    sudo_prefix=""
  else
    warn deps "unknown package manager: install git curl cmake ninja gcc flex bison gperf python3-venv mtools nodejs yourself"
    return 0
  fi
  say "packages: $pkgs"
  if confirm "Install these with sudo?"; then
    bash -c "${sudo_prefix:-true &&} $pm $pkgs" > "$REPORT/deps-packages.log" 2>&1 \
      && pass "deps: packages" || fail "deps: packages" "see deps-packages.log"
  else
    warn "deps: packages" "skipped by you"
  fi
}

ensure_rust() {
  if ! have cargo; then
    [ -x "$HOME/.cargo/bin/cargo" ] || {
      say "installing Rust with rustup"
      curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal > "$REPORT/deps-rust.log" 2>&1
    }
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
  rustup component add rustfmt clippy >> "$REPORT/deps-rust.log" 2>&1
  have cargo && pass "deps: rust" "$(rustc --version)" || fail "deps: rust"
}

# kode-sim looks for ~/.espressif/tools/activate_idf_v6.1.sh (Espressif's installer),
# then $IDF_PATH/export.sh. We use an existing one, or put a git checkout in $WORK.
ensure_idf() {
  if [ -f "$HOME/.espressif/tools/activate_idf_v6.1.sh" ]; then
    pass "deps: esp-idf" "Espressif installer's v6.1 found"; return
  fi
  if [ -n "${IDF_PATH:-}" ] && [ -f "$IDF_PATH/export.sh" ]; then
    pass "deps: esp-idf" "IDF_PATH=$IDF_PATH"; return
  fi
  if [ ! -f "$IDF_DIR/export.sh" ]; then
    say "cloning ESP-IDF v6.1 (about 800 MB)"
    git clone -q -b v6.1 --depth 1 --recursive --shallow-submodules \
      https://github.com/espressif/esp-idf.git "$IDF_DIR" > "$REPORT/deps-idf.log" 2>&1 \
      || { fail "deps: esp-idf" "clone failed, see deps-idf.log"; return; }
  fi
  say "installing ESP-IDF tools for esp32p4 and esp32c5"
  "$IDF_DIR/install.sh" esp32p4,esp32c5 >> "$REPORT/deps-idf.log" 2>&1 \
    || { fail "deps: esp-idf" "install.sh failed, see deps-idf.log"; return; }
  export IDF_PATH="$IDF_DIR"
  pass "deps: esp-idf" "IDF_PATH=$IDF_DIR (add 'export IDF_PATH=$IDF_DIR' to your shell rc)"
}

check_browser() {
  local b=""
  for n in google-chrome google-chrome-stable chromium chromium-browser microsoft-edge brave-browser; do
    have "$n" && { b="$(command -v "$n")"; break; }
  done
  if [ -z "$b" ]; then
    warn "deps: browser" "no Chromium-family browser on PATH: the window opens as a tab in your default browser"
  elif [[ "$(readlink -f "$b")" == /snap/* || "$b" == /snap/* ]]; then
    warn "deps: browser" "$b is a snap: its confinement may refuse kode-sim's profile in ~/.cache - watch the window stage"
  else
    pass "deps: browser" "$b"
  fi
}

# ---------------------------------------------------------------- build
stage_build() {
  if [ "$IN_TREE" = 1 ]; then
    : # this checkout, as it is
  elif [ ! -d "$SIM/.git" ]; then
    git clone -q ${BRANCH:+-b "$BRANCH"} "$REPO" "$SIM" > "$REPORT/build.log" 2>&1 \
      || { fail build "clone of $REPO failed"; return 1; }
  else
    git -C "$SIM" fetch -q origin >> "$REPORT/build.log" 2>&1
    [ -n "$BRANCH" ] && git -C "$SIM" checkout -q "$BRANCH" >> "$REPORT/build.log" 2>&1
    git -C "$SIM" pull -q --ff-only >> "$REPORT/build.log" 2>&1 || true
  fi
  echo "- kode-sim commit: $(git -C "$SIM" log -1 --format='%h %s')" >> "$REPORT/machine.md"
  (cd "$SIM" && cargo build --release) >> "$REPORT/build.log" 2>&1 \
    || { fail build "cargo build failed, see build.log"; return 1; }
  mkdir -p "$HOME/.local/bin"
  ln -sf "$SIM/target/release/kode-sim" "$HOME/.local/bin/kode-sim"
  export PATH="$HOME/.local/bin:$PATH"
  pass build "$(git -C "$SIM" rev-parse --short HEAD)"
}

# ---------------------------------------------------------------- host (CI parity)
stage_host() {
  cd "$SIM" || return 1
  cargo fmt --all --check > "$REPORT/host-fmt.log" 2>&1 && pass "host: rustfmt" || fail "host: rustfmt" "see host-fmt.log"
  cargo clippy --all-targets -- -D warnings > "$REPORT/host-clippy.log" 2>&1 \
    && pass "host: clippy" || fail "host: clippy" "see host-clippy.log"
  [ -x .venv/bin/pytest ] || { python3 -m venv .venv && .venv/bin/pip install -q -r tests/requirements.txt; } \
    > "$REPORT/host-venv.log" 2>&1
  if tools/host_tests.sh > "$REPORT/host-tests.log" 2>&1; then
    pass "host: host_tests.sh" "$(grep -E '[0-9]+ passed' "$REPORT/host-tests.log" | tail -1)"
  else
    fail "host: host_tests.sh" "see host-tests.log"
  fi
  cd - >/dev/null || return
}

# ---------------------------------------------------------------- boot
# A board binary needs no app build: KodeOS's factory pack, merged as docs/GUIDE.md says.
fetch_kodeos() {
  local dir="$WORK/kodeos-release" ver url
  mkdir -p "$dir"
  ver="$(curl -fsSL -o /dev/null -w '%{url_effective}' https://github.com/kodediy/KodeOS/releases/latest 2>/dev/null)"
  ver="${ver##*/v}"; [[ "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || ver="0.1.1"
  echo "- KodeOS release: v$ver" >> "$REPORT/machine.md"
  if [ ! -f "$dir/merged-v$ver.bin" ]; then
    url="https://github.com/kodediy/KodeOS/releases/download/v$ver/kodeos-factory-v$ver.zip"
    curl -fsSL "$url" -o "$dir/factory.zip" || { echo "download failed: $url" >&2; return 1; }
    rm -rf "$dir/pack" && unzip -q -o "$dir/factory.zip" -d "$dir/pack"
    [ -x "$dir/venv/bin/python" ] || { python3 -m venv "$dir/venv" && "$dir/venv/bin/pip" install -q esptool; }
    local p; p="$(dirname "$(find "$dir/pack" -name bootloader.bin | head -1)")"
    "$dir/venv/bin/python" -m esptool --chip esp32p4 merge-bin --pad-to-size 32MB -o "$dir/merged-v$ver.bin" \
      0x2000 "$p/bootloader.bin" 0x8000 "$p/partition-table.bin" 0x1B000 "$p/ota_data_initial.bin" \
      0x20000 "$p/kodeos.bin" 0xC40000 "$p/apps.bin" 0x1800000 "$p/storage.bin" >/dev/null || return 1
  fi
  echo "$dir/merged-v$ver.bin"
}

stage_boot() {
  local img run log=$REPORT/boot.log
  img="$(fetch_kodeos 2> "$REPORT/boot-fetch.log")" || { fail boot "could not fetch/merge KodeOS, see boot-fetch.log"; return; }
  run="$REPORT/kodeos-run.bin"; cp "$img" "$run"   # kode-sim saves the flash back into this file
  local args=(run --firmware "$run" --no-open --port 0)
  [ "$NO_WIFI" = 1 ] && args+=(--no-wifi)
  say "booting KodeOS headless for up to ${BOOT_SECS}s (the first run also builds the C5 firmware)"
  "$KS" "${args[@]}" > "$log" 2>&1 < /dev/null &
  local pid=$! t0="" t1="" frame_ms=""
  local deadline=$((SECONDS + BOOT_SECS + 600))   # + time for the one-off C5 build
  while kill -0 "$pid" 2>/dev/null && [ $SECONDS -lt $deadline ]; do
    [ -z "$t0" ] && grep -q 'Starting emulation (chip=esp32p4' "$log" && t0=$(date +%s%N) && deadline=$((SECONDS + BOOT_SECS))
    if [ -n "$t0" ] && [ -z "$t1" ] && frame_ms=$(grep -oP 'kgui first frame \K[0-9]+' "$log" | head -1) && [ -n "$frame_ms" ]; then
      t1=$(date +%s%N)
    fi
    # with WiFi, wait until the C5 answers or gives up; without, the console is enough
    if [ -n "$t1" ] && grep -q 'kcli console up' "$log"; then
      [ "$NO_WIFI" = 1 ] && break
      grep -qE 'no CP_INIT from the C5|kdrv.radio link up' "$log" && { sleep 15; break; }
    fi
    sleep 0.5
  done
  kill -TERM "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  cp "$(kode_cache)/c5.log" "$REPORT/boot-c5.log" 2>/dev/null
  pgrep -f "esp-emu.*$run" >/dev/null && fail "boot: shutdown" "an esp-emu outlived kode-sim" && pkill -f "esp-emu.*$run"

  grep -q 'the C5.s firmware did not build' "$log" && {
    cp "$(kode_cache)"/kode_c5-*/build.log "$REPORT/boot-c5-build.log" 2>/dev/null
    fail "boot: C5 firmware build" "see boot-c5-build.log (try --no-wifi to test the rest)"; return; }
  grep -q 'making a card image' "$log" && ! grep -q 'kdrv.sd card mounted' "$log" && fail "boot: microSD" "card made but not mounted"
  grep -q 'kdrv.sd card mounted' "$log" && pass "boot: microSD" "$(grep -oP 'kdrv.sd card mounted at \K.*' "$log" | head -1)"
  if [ -n "$t1" ]; then
    local wall_ms=$(( (t1 - t0) / 1000000 ))
    pass "boot: screen" "first frame at ${frame_ms} ms emulated, ${wall_ms} ms wall (speed ~$(awk "BEGIN{printf \"%.2f\", $frame_ms/$wall_ms}")x real time)"
  else
    fail "boot: screen" "no 'kgui first frame' - see boot.log"
  fi
  grep -q 'kcli console up' "$log" && pass "boot: console" || fail "boot: console"
  grep -qE 'Network backend: TAP' "$log" && warn "boot: network" "the P4's esp-emu opened a TAP device (no --net passed to it)"
  if [ "$NO_WIFI" = 0 ]; then
    if grep -q 'kdrv.radio link up' "$log"; then pass "boot: C5 / WiFi" "$(grep -oP 'kdrv.radio \Klink up.*' "$log" | head -1)"
    elif grep -q 'no CP_INIT from the C5' "$log"; then fail "boot: C5 / WiFi" "the P4 never heard the C5 - see boot-c5.log"
    else fail "boot: C5 / WiFi" "no link up within ${BOOT_SECS}s - see boot.log, boot-c5.log"; fi
  fi
  grep -E '^!E ' "$log" | grep -vE 'kdrv.nfc|sdmmc|sdio_wrapper|H_SDIO' | sort -u | head -20 > "$REPORT/boot-errors.txt"
  [ -s "$REPORT/boot-errors.txt" ] && warn "boot: firmware errors" "$(wc -l < "$REPORT/boot-errors.txt") distinct, see boot-errors.txt"
}

kode_cache() { echo "${KODE_SIM_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/kode-sim}"; }

# ---------------------------------------------------------------- emu
stage_emu() {
  cd "$SIM" || return 1
  [ -x .venv/bin/pytest ] || { python3 -m venv .venv && .venv/bin/pip install -q -r tests/requirements.txt; } >/dev/null 2>&1
  say "emulator suite: builds kodeOS, kode-sdk examples and the C5 firmware, boots each (slow)"
  local t=$SECONDS
  .venv/bin/pytest -m emu tests -v -rA --junitxml="$REPORT/emu-junit.xml" > "$REPORT/emu.log" 2>&1
  local rc=$? line; line="$(tail -1 "$REPORT/emu.log")"
  [ $rc = 0 ] && pass "emu: pytest -m emu" "$line ($(( (SECONDS-t)/60 )) min)" \
              || fail "emu: pytest -m emu" "$line - see emu.log / emu-junit.xml"
  grep -E '^(FAILED|ERROR) ' "$REPORT/emu.log" > "$REPORT/emu-failures.txt"
  cd - >/dev/null || return
}

# ---------------------------------------------------------------- window
stage_window() {
  if [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then warn window "no desktop session: skipped"; return; fi
  [ -t 0 ] || { warn window "not interactive: skipped"; return; }
  local app="$WORK/kode-app-template"
  [ -d "$app" ] || git clone -q https://github.com/kodediy/kode-app-template.git "$app"
  cat <<'EOF'

The window stage is by hand. kode-sim builds the app template, boots it and opens the window.
Check, then close the window (or Ctrl-C) to come back here:
  1. the Kode Dot draws, and clicking the screen touches it
  2. T / B and the arrow keys press the buttons
  3. sound plays; Ajustes -> the microphone works
  4. Ajustes opens: battery, USB cable, microSD, motion (tilt and shake)
  5. type `help` in this terminal: the Kode Dot's console answers
  6. join the WiFi network "kode-sim" on the Kode Dot

EOF
  read -r -p "Press Enter to start..." _
  "$KS" run "$app" 2>&1 | tee "$REPORT/window.log"
  read -r -p "Did all six work? [y/N/notes] " a
  case "$a" in
    [Yy]*) pass window "checklist ok" ;;
    *) fail window "${a:-not all}" ;;
  esac
}

# ---------------------------------------------------------------- go
machine_info
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
[ -f "$IDF_DIR/export.sh" ] && [ -z "${IDF_PATH:-}" ] && export IDF_PATH="$IDF_DIR"

if want deps; then pkg_install; ensure_rust; ensure_idf; check_browser; fi
if want build || [ ! -x "$SIM/target/release/kode-sim" ]; then stage_build || STAGES="deps"; fi
want host && stage_host
want boot && stage_boot
want emu && stage_emu
want window && stage_window

{
  cat "$REPORT/machine.md"
  echo
  echo "| stage | result | notes |"
  echo "|---|---|---|"
  printf '%s\n' "${RESULTS[@]}"
  echo
  echo "Logs: $REPORT"
} > "$SUMMARY"

echo; cat "$SUMMARY"
grep -q '\*\*FAIL\*\*' "$SUMMARY" && exit 1 || exit 0
