#!/usr/bin/env bash
# Compila e instala o Lingo para o usuário atual: binário, .desktop e regras do Hyprland.
set -euo pipefail
root="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
bin="$HOME/.local/bin/lingo"

if ! command -v cargo >/dev/null; then
  echo "cargo não encontrado: instale o Rust (https://rustup.rs) e rode de novo" >&2
  exit 1
fi
cargo build --release --manifest-path "$root/Cargo.toml"
[ -L "$bin" ] && rm -f "$bin" # a versão Python instalava um link
install -Dm755 "$root/target/release/lingo" "$bin"
mkdir -p "$HOME/.local/share/applications"
# Caminho absoluto: a sessão do Hyprland pode não ter ~/.local/bin no PATH.
sed "s|@LINGO@|$bin|g" "$root/data/lingo.desktop.in" > "$HOME/.local/share/applications/lingo.desktop"
echo "binário: $bin"

hypr="$HOME/.config/hypr"
if [ -f "$hypr/hyprland.conf" ]; then
  conf="$hypr/lingo.conf"
  sed "s|@LINGO@|$bin|g" "$root/data/hyprland.conf.in" > "$conf"
  # Atalhos SUPER+ALT que já fazem outra coisa (no Omarchy, por exemplo) ficam comentados.
  if command -v hyprctl >/dev/null && hyprctl binds >/dev/null 2>&1; then
    taken="$(hyprctl binds | awk '
      /^bind/ { mod = ""; key = "" }
      /^\tmodmask:/ { mod = $2 }
      /^\tkey:/ { key = $2 }
      /^\targ:/ { if (mod == 72 && $0 !~ /lingo/) print key }')"
    for key in $taken; do
      if grep -q "^bind = SUPER ALT, $key," "$conf"; then
        sed -i "s|^bind = SUPER ALT, $key,|# SUPER+ALT+$key já está em uso: bind = SUPER ALT, $key,|" "$conf"
        echo "atalho SUPER+ALT+$key já está em uso; deixei comentado em $conf"
      fi
    done
  fi
  if ! grep -qF 'source = ~/.config/hypr/lingo.conf' "$hypr/hyprland.conf"; then
    cp "$hypr/hyprland.conf" "$hypr/hyprland.conf.bak-lingo-$(date +%Y%m%d-%H%M%S)"
    printf '\n# Lingo (tradução ao vivo em calls)\nsource = ~/.config/hypr/lingo.conf\n' >> "$hypr/hyprland.conf"
  fi
  echo "hyprland: regras e atalhos em $conf"
fi
