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

- Prompt box, enter to generate.
- Drag & drop (or paste) an image path to attach a reference.
- Follow-up prompts edit the last image; the terminal renders the result inline.
- Saves into whatever folder you are `cd`-ed into.
- `tab` cycles quality · `esc` cancels/clears · `ctrl-c` quits.

## Install

### <img src="https://img.shields.io/badge/-Rust-000000?style=flat-square&logo=rust&logoColor=white" alt="Rust" width="60" /> From source (macOS, Linux, Windows)

```bash
gh repo clone prophesourvolodymyr/fuckinggen
cd fuckinggen
cargo install --path .
```

That gives you both `fgen` and `fuckinggen` in `~/.cargo/bin`.

Prebuilt releases and a Homebrew tap are coming. The logo is coming too — somebody is drawing it.

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

That installs the binaries and drops the `fuckinggen` skill into every skill directory it finds (`~/.agents/skills`, `~/.claude/skills`, `~/.codex/skills`, `~/.gemini/skills`, `~/.cursor/skills`, `~/.config/agents/skills`, `~/.aider-desk/skills`). Agents then call `fgen` themselves.

## Use

```bash
fgen gen "prompt"                      # one image into ~/Downloads
fgen gen "prompt" --out ./thing.png    # exact path
fgen gen "a" "b" --out-dir ./assets    # batch, parallel with -c
fgen gen "edit this" -i ref.png        # reference image / edit
fgen gen "prompt" --quality high       # low | medium | high | auto
fgen tui                               # interactive TUI (fgen -t)
fgen auth                              # token status
fgen --help                            # everything else
```

Existing files are never overwritten: `thing.png` becomes `thing-v2.png`, then `thing-v3.png`, and so on.

`--size` and `--quality` are requests to the backend. Quality usually sticks; the ChatGPT backend often picks the final dimensions itself (you get what it gives you, and the file is real).

## Notes

- The tool talks to the same ChatGPT/Codex backend channel the Codex CLI uses, with your existing subscription login. Unofficial, not affiliated with OpenAI — use it within OpenAI's terms.
- No API key ever touches this tool.
- License: WTFPL.
