#!/usr/bin/env bash
# fuckinggen installer: builds the CLI, installs both binaries, and drops the
# agent skill into every skill directory on this machine.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "==> building fuckinggen"
cargo install --path "$REPO" --force

echo "==> installing the agent skill"
SKILL_ROOTS=(
  "$HOME/.agents/skills"
  "$HOME/.config/agents/skills"
  "$HOME/.claude/skills"
  "$HOME/.codex/skills"
  "$HOME/.gemini/skills"
  "$HOME/.cursor/skills"
  "$HOME/.aider-desk/skills"
)
for skill_root in "${SKILL_ROOTS[@]}"; do
  [ -d "$skill_root" ] || continue
  rm -rf "$skill_root/fuckinggen"
  cp -R "$REPO/skills/fuckinggen" "$skill_root/fuckinggen"
  echo "    -> $skill_root/fuckinggen/SKILL.md"
done

echo "==> done. try: fgen -t"
