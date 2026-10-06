<!-- logo: drop assets/logo.png into the repo and uncomment the block below -->
<!--
<p align="center">
  <img src="assets/logo.png" alt="fuckinggen logo" width="180" />
</p>
-->

<p align="center">
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-000000?style=flat-square&logo=rust&logoColor=white" alt="Built with Rust" /></a>
  <a href="https://github.com/prophesourvolodymyr/fuckinggen/actions/workflows/ci.yml"><img src="https://github.com/prophesourvolodymyr/fuckinggen/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="https://github.com/prophesourvolodymyr/fuckinggen/releases"><img src="https://img.shields.io/github/v/release/prophesourvolodymyr/fuckinggen?display_name=tag&style=flat-square" alt="Latest release" /></a>
  <a href="https://github.com/prophesourvolodymyr/homebrew-fuckinggen"><img src="https://img.shields.io/badge/Homebrew-tap-FBB040?style=flat-square&logo=homebrew&logoColor=white" alt="Homebrew tap" /></a>
  <a href="https://aur.archlinux.org/packages/fgen"><img src="https://img.shields.io/aur/version/fgen?style=flat-square&logo=archlinux&logoColor=white&label=AUR" alt="AUR package" /></a>
  <a href="https://github.com/prophesourvolodymyr/fuckinggen/blob/main/LICENSE"><img src="https://img.shields.io/github/license/prophesourvolodymyr/fuckinggen?style=flat-square" alt="WTFPL license" /></a>
  <img src="https://img.shields.io/badge/macOS-supported-000000?style=flat-square&logo=apple&logoColor=white" alt="macOS supported" />
  <img src="https://img.shields.io/badge/Linux-supported-FCC624?style=flat-square&logo=linux&logoColor=black" alt="Linux supported" />
  <img src="https://img.shields.io/badge/ChatGPT-subscription-10A37F?style=flat-square&logo=openai&logoColor=white" alt="Runs on your ChatGPT subscription" />
</p>

<h1 align="center">fuckinggen</h1>

<h2 align="center">Generate images. Fucking generate them.</h2>

<p align="center">Images from your own ChatGPT subscription, straight from the terminal. CLI and a real TUI.</p>

`fgen` makes images with the ChatGPT account already logged in on your machine. No API key, no per-image API invoice — it rides the subscription you already pay for, on the current image model (ChatGPT Images 2.5). `fuckinggen` is the same binary with the longer name.

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
- Every generation lands in a gallery: `↑`/`↓` (or `ctrl-p`/`ctrl-n`) walk through them, the title shows `index/total` and the file size.
- While it generates, a snake board appears below the progress bar. Click it, steer with the arrows, edges wrap.
- Type `/` for the command palette above the prompt bar (arrows pick, tab completes, enter runs). Saves into the folder you are `cd`-ed in — `/root` moves everything to `~/Downloads` instead.
- Blue by default; `/settings` cycles the theme, flips quality, and logs you into Codex.

Slash commands: `/open` (system image viewer) · `/view` (reveal in the file manager) · `/root` · `/remove` · `/remove-all` · `/quality` · `/dir` · `/settings` · `/help` · `/clear` · `/quit`

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

## Windows

Not supported yet. There is no Windows build, and the open/reveal paths are macOS and Linux only.

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

That installs the binaries and drops the `gpt-image-gen-latest` skill into every skill directory it finds (`~/.agents/skills`, `~/.claude/skills`, `~/.codex/skills`, `~/.gemini/skills`, `~/.cursor/skills`, `~/.config/agents/skills`, `~/.aider-desk/skills`). Agents then call `fgen` themselves. From Homebrew? Grab `install.sh` and `skills/` from the repo, or just run it in a clone.

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
- macOS and Linux are the supported platforms today.

## License

WTFPL
