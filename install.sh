#!/usr/bin/env bash
# fuckinggen installer: builds the CLI, installs both binaries, and drops the
# agent skill into every skill directory on this machine.
#
#   ./install.sh                 # build + install binaries, then the skill
#   ./install.sh --skills-only   # skip the build (package managers did it)
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

skills_only=0
for arg in "$@"; do
  case "$arg" in
    --skills-only|--skills) skills_only=1 ;;
    -h|--help)
      echo "usage: install.sh [--skills-only]"
      exit 0
      ;;
    *)
      echo "unknown flag: $arg (try --help)" >&2
      exit 2
      ;;
  esac
done

if [ "$skills_only" -eq 0 ]; then
  echo "==> building fuckinggen"
  cargo install --path "$REPO" --force
fi

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
  rm -rf "$skill_root/gpt-image-gen-latest"
  cp -R "$REPO/skills/gpt-image-gen-latest" "$skill_root/gpt-image-gen-latest"
  echo "    -> $skill_root/gpt-image-gen-latest/SKILL.md"
done

echo "==> done. try: fgen -t"
