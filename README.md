<!-- logo: drop assets/logo.png into the repo and uncomment the block below -->
<!--
<p align="center">
  <img src="assets/logo.png" alt="fuckinggen logo" width="180" />
</p>
-->

<p align="center">
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-000000?style=flat-square&logo=rust&logoColor=white" alt="Built with Rust" /></a>
  <a href="https://github.com/prophesourvolodymyr/fuckinggen/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-WTFPL-blue?style=flat-square" alt="WTFPL license" /></a>
  <img src="https://img.shields.io/badge/ChatGPT-subscription-10A37F?style=flat-square&logo=openai&logoColor=white" alt="Runs on your ChatGPT subscription" />
  <img src="https://img.shields.io/badge/macOS-supported-000000?style=flat-square&logo=apple&logoColor=white" alt="macOS supported" />
  <img src="https://img.shields.io/badge/Linux-supported-FCC624?style=flat-square&logo=linux&logoColor=black" alt="Linux supported" />
  <img src="https://img.shields.io/badge/Windows-supported-0078D4?style=flat-square&logo=windows&logoColor=white" alt="Windows supported" />
</p>

<h1 align="center">fuckinggen</h1>

<h2 align="center">Generate images. Fucking generate them.</h2>

<p align="center">Image generation from your own ChatGPT subscription, straight from the terminal. CLI and a real TUI.</p>

`fgen` (or `fuckinggen`, same binary) makes images with the ChatGPT account already logged in on your machine. No API key, no per-image API invoice — it rides the subscription you already pay for, on the current image model (ChatGPT Images 2.5). It also has a TUI where you type a prompt, drop reference images in, and watch the thing come back rendered in your terminal.

It is called `fuckinggen` because that is the fucking name.

## See it work

**CLI — one prompt, one image**

```bash
fgen gen "a red square on a white background, flat vector"
# -> /Users/you/Downloads/a-red-square-on-a-white-background.png
```

**Batch + progress**

```bash
fgen gen "poster A" "poster B" --out-dir ./assets --concurrency 2
# [job 1/2] generating (7.4s)
# [job 2/2] generating (7.9s)
# /path/assets/poster-a.png
# /path/assets/poster-b.png
```

**Edits with reference images**

```bash
fgen gen "recolor the square to blue" --images red-square.png --out blue.png
```

**Machine-readable mode** for agents and scripts:

```bash
fgen gen "hero shot" --json
# {"event":"start",...}
# {"event":"phase","phase":"generating",...}
# {"event":"saved","path":"/.../hero-shot.png","bytes":912344,...}
# {"event":"summary","ok":true,...}
```

**TUI** — `fgen -t`

- Prompt box, enter to generate; `shift`+`enter` adds a line and the box grows with your text.
- Drag & drop (or paste) image paths — one or many, quoted or escaped — and they attach as references: previews show up in a `refs` strip right above the prompt bar, click one to drop it. A path never fires a generation by accident.
- Every generation lands in a gallery: `↑`/`↓` (or `ctrl-p`/`ctrl-n`) walk through them, the panel title shows `index/total` and the file size.
- While an image is generating, a small snake board appears below the progress bar: click it, then use the arrow keys; crossing an edge wraps to the opposite side.
- Saves into whatever folder you are `cd`-ed into — `/root` moves everything to `~/Downloads` instead.
- Blue by default; `/settings` cycles the theme, flips quality, and logs you into Codex.

Slash commands: `/open` (system image viewer) · `/view` (reveal in the file manager) · `/root` · `/remove` · `/remove-all` · `/quality` · `/dir` · `/settings` · `/help` · `/clear` · `/quit`

## Install

### <img src="https://img.shields.io/badge/-Homebrew-FBB040?style=flat-square&logo=homebrew&logoColor=white" alt="Homebrew" width="110" /> Homebrew (macOS and Linux)

```bash
brew tap prophesourvolodymyr/fuckinggen
brew install fuckinggen
```

Both `fgen` and `fuckinggen` land in your Homebrew bin. The tap builds from source and declares Rust as a build dependency, so Homebrew installs the compiler automatically.

### <img src="https://img.shields.io/badge/-Rust-000000?style=flat-square&logo=rust&logoColor=white" alt="Rust" width="60" /> Cargo

```bash
cargo install --git https://github.com/prophesourvolodymyr/fuckinggen
```

Or from a checkout: `gh repo clone prophesourvolodymyr/fuckinggen && cd fuckinggen && cargo install --path .` — puts both binaries in `~/.cargo/bin`.

### Manual downloads

The [Releases](https://github.com/prophesourvolodymyr/fuckinggen/releases) page contains the source archive and any available prebuilt target archives (`fuckinggen-vX.Y.Z-<target>.tar.gz`, containing both binaries plus README and LICENSE).

### Linux notes

- The release workflow targets Linux x86_64 and aarch64; the TUI falls back to half-block rendering when the terminal has no inline-image protocol.
- Inline image previews use whatever the terminal supports (Kitty graphics, iTerm2 inline images, Sixel, Ghostty/WezTerm) and fall back to coloured half-blocks everywhere else.
- `/open` uses `xdg-open`; `/view` uses the `org.freedesktop.FileManager1` D-Bus interface (Nautilus, Dolphin, Nemo, Thunar) and falls back to opening the folder.
- `codex login` works the same; the token lives in `~/.codex/auth.json`.

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

That installs the binaries and drops the `gpt-image-gen-latest` skill into every skill directory it finds (`~/.agents/skills`, `~/.claude/skills`, `~/.codex/skills`, `~/.gemini/skills`, `~/.cursor/skills`, `~/.config/agents/skills`, `~/.aider-desk/skills`). Agents then call `fgen` themselves. (Homebrew users: grab `install.sh` and `skills/` from the repo, or just run it from a clone.)

## Use

```bash
fgen gen "prompt"                      # one image into ~/Downloads
fgen gen "prompt" --out ./thing.png    # exact path
fgen gen "a" "b" --out-dir ./assets    # batch, parallel with -c
fgen gen "edit this" -i ref.png        # reference image / edit
fgen gen "make it blue" --last         # follow-up edit of the newest generation
fgen last -n 5                         # list recent generations
fgen gen "prompt" --quality high       # low | medium | high | auto
fgen tui                               # interactive TUI (fgen -t)
fgen auth                              # token status
fgen --help                            # everything else
```

Every finished image is remembered (newest first, last 50) in `~/.local/state/fuckinggen/state.json` — that is what `--last` and `fgen last` read, and it is how follow-up edits of "the thing you just made" work without hunting for paths.

Existing files are never overwritten: `thing.png` becomes `thing-v2.png`, then `thing-v3.png`, and so on.

`--size` and `--quality` are requests to the backend. Quality usually sticks; the ChatGPT backend often picks the final dimensions itself (you get what it gives you, and the file is real).

## Notes

- The tool talks to the same ChatGPT/Codex backend channel the Codex CLI uses, with your existing subscription login. Unofficial, not affiliated with OpenAI — use it within OpenAI's terms.
- No API key ever touches this tool.
- License: WTFPL.
