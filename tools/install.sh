#!/usr/bin/env bash
# Instala o Lingo para o usuário atual: comando no PATH, .desktop e regras do Hyprland.
set -euo pipefail
root="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
launcher="$root/bin/lingo"

mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
ln -sfn "$launcher" "$HOME/.local/bin/lingo"
# Caminho absoluto: a sessão Hyprland não tem ~/.local/bin no PATH.
sed "s|@LINGO@|$launcher|g" "$root/data/lingo.desktop.in" > "$HOME/.local/share/applications/lingo.desktop"
echo "lançador: ~/.local/bin/lingo e ~/.local/share/applications/lingo.desktop"

hypr="$HOME/.config/hypr"
if [ -f "$hypr/hyprland.conf" ]; then
  sed "s|@LINGO@|$launcher|g" "$root/data/hyprland.conf.in" > "$hypr/lingo.conf"
  if ! grep -qF 'source = ~/.config/hypr/lingo.conf' "$hypr/hyprland.conf"; then
    cp "$hypr/hyprland.conf" "$hypr/hyprland.conf.bak-lingo-$(date +%Y%m%d-%H%M%S)"
    printf '\n# Lingo (tradução ao vivo em calls)\nsource = ~/.config/hypr/lingo.conf\n' >> "$hypr/hyprland.conf"
  fi
  echo "hyprland: regras e atalhos em ~/.config/hypr/lingo.conf"
fi
