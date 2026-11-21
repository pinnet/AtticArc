#!/usr/bin/env bash
# Sets up a fresh Ubuntu desktop for building and testing AtticARC (and Plugin²), then
# builds and tests AtticARC. Safe to run again: it only adds what is missing, and pulls.
#
#   curl -fsSL https://raw.githubusercontent.com/pinnet/AtticArc/main/scripts/linux-test.sh | bash
#   bash scripts/linux-test.sh [branch] [--plugin2]
#
# Branch: main unless given. --plugin2 also clones and tests pinnet/plugin-squared.
# Everything goes in ~/dev; nothing outside your home folder but the apt packages.

set -euo pipefail
BRANCH=main
PLUGIN2=0
for a in "$@"; do
  case "$a" in
    --plugin2) PLUGIN2=1 ;;
    -*) echo "unknown option $a"; exit 2 ;;
    *) BRANCH="$a" ;;
  esac
done
say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

say "1. Build tools and the libraries the GUI needs (asks for your password once)"
sudo apt-get update -qq
sudo apt-get install -y -qq build-essential pkg-config git curl \
  libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev libxrandr-dev \
  libxi-dev libgl1-mesa-dev libgtk-3-dev >/dev/null
echo "  ok"

say "2. Rust"
if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile default
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
rustup update stable >/dev/null 2>&1 || true
echo "  ok: $(rustc --version)"

fetch() { # repo dir branch
  if [ -d "$2/.git" ]; then
    GIT_SSH_COMMAND="ssh -o StrictHostKeyChecking=accept-new" git -C "$2" fetch -q origin &&
      git -C "$2" checkout -q "$3" &&
      GIT_SSH_COMMAND="ssh -o StrictHostKeyChecking=accept-new" git -C "$2" pull -q --ff-only
  else
    # Public repos over HTTPS; a private one (plugin-squared) over SSH, with the
    # key you added to GitHub. GitHub's host key is accepted the first time.
    GIT_TERMINAL_PROMPT=0 git clone -q -b "$3" "https://github.com/$1.git" "$2" 2>/dev/null ||
      GIT_SSH_COMMAND="ssh -o StrictHostKeyChecking=accept-new" git clone -q -b "$3" "git@github.com:$1.git" "$2" ||
      { echo "  can't clone $1: for a private repo, add this VM's SSH key to GitHub (ssh -T git@github.com to check)"; exit 1; }
  fi
  echo "  ok: $(git -C "$2" log --oneline -1)"
}

mkdir -p "$HOME/dev"
say "3. AtticARC ($BRANCH)"
fetch pinnet/AtticArc "$HOME/dev/AtticArc" "$BRANCH"
cd "$HOME/dev/AtticArc"

say "4. Tests"
log="$HOME/dev/atticarc-test.log"
if cargo test --workspace >"$log" 2>&1; then
  echo "  all passed ($(grep -c '^test result: ok' "$log") test groups)"
else
  grep -E "FAILED|panicked|^error" "$log" | head -20
  echo "  SOME FAILED: the whole output is in $log"
fi

say "5. Release build"
cargo build --release --workspace -q && echo "  ok: ~/dev/AtticArc/target/release/atticarc"

if [ "$PLUGIN2" = 1 ]; then
  say "6. Plugin²"
  fetch pinnet/plugin-squared "$HOME/dev/plugin-squared" main
  (cd "$HOME/dev/plugin-squared" && cargo test --workspace -q >/dev/null 2>&1 && echo "  tests passed" || echo "  SOME FAILED: cd ~/dev/plugin-squared && cargo test --workspace")
  (cd "$HOME/dev/plugin-squared" && cargo build --release --workspace -q && echo "  ok: ~/dev/plugin-squared/target/release/plugin2")
fi

say "Done. To try the GUI:"
echo "  ~/dev/AtticArc/target/release/atticarc"
echo "  (if the window is blank or doesn't open in a VM:  ~/dev/AtticArc/target/release/atticarc --compatible-graphics)"
