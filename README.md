<p align="center">
  <img src="assets/logo.png" alt="fgen logo" width="180" />
</p>

<p align="center">
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-000000?style=flat-square&logo=rust&logoColor=white" alt="Built with Rust" /></a>
  <a href="https://github.com/prophesourvolodymyr/fuckinggen/actions/workflows/ci.yml"><img src="https://github.com/prophesourvolodymyr/fuckinggen/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="https://github.com/prophesourvolodymyr/fuckinggen/releases"><img src="https://img.shields.io/github/v/release/prophesourvolodymyr/fuckinggen?display_name=tag&style=flat-square" alt="Latest release" /></a>
  <a href="https://github.com/prophesourvolodymyr/homebrew-fuckinggen"><img src="https://img.shields.io/badge/Homebrew-tap-FBB040?style=flat-square&logo=homebrew&logoColor=white" alt="Homebrew tap" /></a>
  <a href="https://aur.archlinux.org/packages/fgen"><img src="https://img.shields.io/aur/version/fgen?style=flat-square&logo=archlinux&logoColor=white&label=AUR" alt="AUR package" /></a>
  <a href="https://github.com/prophesourvolodymyr/fuckinggen/blob/main/LICENSE"><img src="https://img.shields.io/github/license/prophesourvolodymyr/fuckinggen?style=flat-square" alt="WTFPL license" /></a>
  <img src="https://img.shields.io/badge/macOS-supported-000000?style=flat-square&logo=apple&logoColor=white" alt="macOS supported" />
  <img src="https://img.shields.io/badge/Linux-supported-FCC624?style=flat-square&logo=linux&logoColor=black" alt="Linux supported" />
  <img src="https://img.shields.io/badge/Windows-supported-0078D4?style=flat-square&logo=windows&logoColor=white" alt="Windows supported" />
  <img src="https://img.shields.io/badge/ChatGPT-subscription-10A37F?style=flat-square&logo=openai&logoColor=white" alt="Runs on your ChatGPT subscription" />
</p>

<h1 align="center">fuckinggen</h1>

<h2 align="center">Generate images. Fucking generate them.</h2>

<p align="center">Images from your own ChatGPT subscription, straight from the terminal. CLI and a real TUI.</p>

## See it work

<p align="center"><a href="#install">Don't Care - Download this Fucker Now</a></p>

### One prompt, one image

```bash
fgen gen "a red square on a white background, flat vector"
# -> /Users/you/Downloads/a-red-square-on-a-white-background.png
```

### Batch the fuck out of it

```bash
fgen gen "poster A" "poster B" --out-dir ./assets --concurrency 2
# [job 1/2] generating (7.4s)
# [job 2/2] generating (7.9s)
# /path/assets/poster-a.png
# /path/assets/poster-b.png
```

### Edits with reference images

```bash
fgen gen "recolor the square to blue" --images red-square.png --out blue.png
fgen gen "make it blue" --last      # follow-up edit of the newest generation
```

### Machine-readable mode

For agents and scripts:

```bash
fgen gen "hero shot" --json
# {"event":"start",...}
# {"event":"phase","phase":"generating",...}
# {"event":"saved","path":"/.../hero-shot.png","bytes":912344,...}
# {"event":"summary","ok":true,...}
```

### Rich TUI

```bash
fgen -t
```

- Prompt box. `enter` generates, `shift`+`enter` adds a line and the box grows with your text.
- Drag & drop (or paste) image paths — one or many, quoted or escaped — and they attach as references: previews show up in the `refs` strip above the prompt bar, click one to drop it. A path never fires a generation by accident.
- Every generation lands in a gallery: `↑`/`↓` (or `ctrl-p`/`ctrl-n`) walk through them, the title shows `index/total`, the file size, and whether it is `saved` or `unsaved`.
- **Follow-ups keep the picture.** The next prompt carries the image you are looking at back to the model, so "change the cup to a cucumber" edits it instead of generating a lonely cucumber. `/ctx` turns that off for a fresh start; with the CLI it is `--last`.
- While it generates, a big square snake board fills the middle of the output panel and is already listening: **arrows steer it right away** (no click, no focus), the body tapers from head to tail, and the edges wrap. `esc` pauses it; click the board to pick it back up.
- Type `/` for the command palette above the prompt bar (arrows pick, tab completes, enter runs).

**Nothing lands in your folders until you say so.** A session generates into a scratch cache, and the way out is a checklist:

```bash
ctrl-c        # keep/discard checklist: space ticks, enter keeps the ticked ones, n keeps nothing
ctrl-c ctrl-c # impatient? the second one quits and leaves the staged files in the cache (it prints where)
enter         # on the gallery: keep the image you are looking at right now
```

Ticked images move into the save directory — the folder you are `cd`-ed in, or `~/Downloads` after `/root` — with the usual never-overwrite naming. Unticked ones are deleted, and a session that already saved everything skips the questions entirely. `[finish]` in the control bar opens the same checklist, and `esc` inside it goes back to work.

- Blue by default; `/settings` cycles the theme, flips quality, and logs you into Codex.

Slash commands: `/open` (system image viewer) · `/view` (reveal in the file manager) · `/save` · `/save-all` · `/ctx` · `/root` · `/remove` · `/remove-all` · `/quality` · `/dir` · `/settings` · `/help` · `/clear` · `/quit`

Transparent renders stay transparent in Kitty and iTerm2 terminals (they carry an alpha channel); everywhere else the picture is composited onto your terminal background instead of turning into a black box.

# Install

## <img src="./assets/platform-apple.svg" alt="Apple" width="18" height="18" /> macOS

**Homebrew**

```bash
brew tap prophesourvolodymyr/fuckinggen
brew install fuckinggen
```

**Manual:** download `…-aarch64-apple-darwin.tar.gz` (Apple silicon) or `…-x86_64-apple-darwin.tar.gz` (Intel) from [Releases](https://github.com/prophesourvolodymyr/fuckinggen/releases/latest). Each archive holds both binaries plus the README and LICENSE.

---

## <img src="./assets/platform-linux.svg" alt="Linux" width="18" height="18" /> Linux

**Arch Linux / AUR**

```bash
yay -S fgen
```

Both binaries land in `/usr/bin`. The agent skill and `install.sh` ship under `/usr/share/fgen/`, so agents can pick it up with:

```bash
sudo bash /usr/share/fgen/install.sh --skills-only
```

**Homebrew**

```bash
brew tap prophesourvolodymyr/fuckinggen
brew install fuckinggen
```

Homebrew builds it from source and pulls Rust in as a build dependency.

**Manual:** grab the Linux archive from [Releases](https://github.com/prophesourvolodymyr/fuckinggen/releases/latest) — the release workflow builds x86_64 and aarch64.

Everything on Linux:

- Inline previews use whatever your terminal speaks (Kitty, iTerm2, Sixel, Ghostty, WezTerm) and fall back to coloured half-blocks everywhere else.
- `/open` uses `xdg-open`; `/view` asks `org.freedesktop.FileManager1` to reveal the file (Nautilus, Dolphin, Nemo, Thunar) and falls back to opening the folder.
- `codex login` works the same; the token lives in `~/.codex/auth.json`.

---

## <img src="./assets/platform-windows.svg" alt="Windows" width="18" height="18" /> Windows

**Manual:** download the Windows zip (`…-x86_64-pc-windows-*.zip`) from [Releases](https://github.com/prophesourvolodymyr/fuckinggen/releases/latest), unzip it somewhere permanent, and add that folder to your `PATH`. It contains `fgen.exe`, `fuckinggen.exe` (same program), the README, the license, and the agent skill.

Everything on Windows:

- `/open` uses `start`, `/view` reveals the file with `explorer /select,`.
- Windows Terminal shows the TUI in full colour; inline previews fall back to half-blocks.
- The progress line switches to plain output on consoles without ANSI/VT support.
- `codex login` works the same; the token lives in `%USERPROFILE%\.codex\auth.json`. `~` expands to `%USERPROFILE%` (or `%HOMEDRIVE%%HOMEPATH%`).
- The agent skill ships as `skills\gpt-image-gen-latest` in the zip: copy it into your agent's skills folder by hand (`install.sh` is bash-only, so it is for macOS and Linux).

---

### Auth (once)

`fuckinggen` does not log in anywhere. It reads the ChatGPT OAuth token your Codex CLI already stored:

```bash
codex login        # if you are not logged in yet
fgen auth          # shows the token, the account, and when it expires
```

### Agent skill (whole machine)

Every agent on this machine can learn to use it:

```bash
./install.sh
```

That installs the binaries and drops the `gpt-image-gen-latest` skill into every skill directory it finds (`~/.agents/skills`, `~/.claude/skills`, `~/.codex/skills`, `~/.gemini/skills`, `~/.cursor/skills`, `~/.config/agents/skills`, `~/.aider-desk/skills`). Agents then call `fgen` themselves.

Installed from a package manager? Both the AUR package and the Homebrew formula ship `install.sh` plus the skill, so you can skip the build:

```bash
sudo bash /usr/share/fgen/install.sh --skills-only                       # AUR
bash "$(brew --prefix fuckinggen)/share/fgen/install.sh" --skills-only   # Homebrew
```

<p align="center">
  <a href="https://buymeacoffee.com/professorvolodymyr"><img src="https://img.buymeacoffee.com/button-api/?text=Buy%20me%20a%20coffee&emoji=%E2%98%95&slug=professorvolodymyr&button_colour=D4FF45&font_colour=0B28B6&font_family=Inter&outline_colour=0B28B6&coffee_colour=FFDD00" alt="Buy me a coffee" /></a>
</p>

## Use

```bash
fgen gen "prompt"                      # one image into ~/Downloads
fgen gen "prompt" --out ./thing.png    # exact path
fgen gen "a" "b" --out-dir ./assets    # batch, parallel with -c
fgen gen "edit this" -i ref.png        # reference image / edit
fgen gen "make it blue" --last         # follow-up edit of the newest generation
fgen gen "keep going" --last 2         # ...or the second newest
fgen last -n 5                         # list recent generations
fgen gen "prompt" --quality high       # low | medium | high | auto
fgen tui                               # interactive TUI (fgen -t)
fgen auth                              # token status
fgen --help                            # everything else
```

Every finished image is remembered (newest first, last 50) in `~/.local/state/fuckinggen/state.json` — that is what `--last` and `fgen last` read, and how follow-up edits of "the thing you just made" work without hunting for paths.

Existing files are never overwritten: `thing.png` becomes `thing-v2.png`, then `thing-v3.png`, and so on.

`--size` and `--quality` are requests to the backend. Quality usually sticks; the ChatGPT backend often picks the final dimensions itself (you get what it gives you, and the file is real).

## Development

```bash
cargo test
cargo run --bin fgen -- gen "a red square on white"
```

## Notes

- The tool talks to the same ChatGPT/Codex backend channel the Codex CLI uses, with your existing subscription login. Unofficial, not affiliated with OpenAI — use it within OpenAI's terms.
- No API key ever touches this tool.
- macOS, Linux, and Windows are the supported platforms today.

## License

WTFPL
