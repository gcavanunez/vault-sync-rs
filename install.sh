#!/bin/sh
set -eu
PROJECT_DIR=$(CDPATH= cd -P "$(dirname "$0")" && pwd)
case "$(uname -s)" in
  Linux|Darwin) ;;
  *) printf 'Supported systems: Linux and macOS.\n' >&2; exit 1 ;;
esac
BINARY=${1:-"$PROJECT_DIR/target/release/vault-sync"}
BIN_DIR="$HOME/.local/bin"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/vault-sync"
if [ ! -x "$BINARY" ]; then
  printf 'Build first with cargo build --release --locked, or supply a compiled binary path.\n' >&2
  exit 1
fi
if [ -e "$CONFIG_DIR/config" ] && [ ! -e "$CONFIG_DIR/config.json" ]; then
  printf 'Migrate %s/config to config.json using config.example.json first.\n' "$CONFIG_DIR" >&2
  exit 1
fi
mkdir -p "$BIN_DIR" "$CONFIG_DIR"
if [ -e "$BIN_DIR/vault-sync" ] || [ -L "$BIN_DIR/vault-sync" ]; then
  if [ -L "$BIN_DIR/vault-sync" ] || ! cmp -s "$BINARY" "$BIN_DIR/vault-sync"; then
    printf 'Stop the service and move aside %s/vault-sync before upgrading.\n' "$BIN_DIR" >&2
    exit 1
  fi
else
  install -m 755 "$BINARY" "$BIN_DIR/vault-sync"
fi
if [ ! -e "$CONFIG_DIR/config.json" ]; then
  install -m 600 "$PROJECT_DIR/config.example.json" "$CONFIG_DIR/config.json"
fi
"$BIN_DIR/vault-sync" service-install
printf 'Edit %s/config.json before starting the service.\n' "$CONFIG_DIR"
