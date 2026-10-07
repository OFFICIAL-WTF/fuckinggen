---
name: gpt-image-gen-latest
description: Generate or edit images with the user's ChatGPT subscription from the terminal via the fgen/fuckinggen CLI (Codex backend) - text-to-image, reference-image edits, follow-up edits of previous generations, batch jobs, live progress, and an interactive TUI. Use for any request to create or modify raster images (illustrations, photos, posters, mockups, product shots, assets).
---

# GPT Image Gen (fuckinggen CLI)

Terminal image generation on the user's **own ChatGPT subscription** — no API key, no per-image API billing. It calls the same ChatGPT/Codex backend channel the Codex CLI uses, with the login already on this machine. Today's model is **ChatGPT Images 2.5**.

Binary: `fgen` (alias: `fuckinggen`). Source: `~/GSpace/Opensource/WTF/fuckinggen` (GitHub: `prophesourvolodymyr/fuckinggen`).

## When to use

- The user asks for an image: illustration, photo, poster, mockup, texture, sprite, character, product shot, logo concept (raster), meme, diagram art.
- The user asks to modify an existing image ("make it blue", "add a hat", "put both characters together").
- The user wants to keep iterating on something just generated — use `--last` instead of hunting for the file.
- Anything that should end up as a PNG file. For SVG/vector or code-native UI assets, use the project's own design system instead.

## Prerequisites

1. `fgen` on PATH (`which fgen`). Install with Homebrew (`brew tap prophesourvolodymyr/fuckinggen && brew install fuckinggen`), from the AUR (`yay -S fgen` on Arch), or from source (`cd ~/GSpace/Opensource/WTF/fuckinggen && cargo install --path .`).
2. Subscription login: `fgen auth` must report a valid token. If it fails, ask the user to run `codex login`, then `fgen auth` again. **Never** ask for or use an OpenAI API key.

## Commands

```bash
fgen gen "a red square on white"                  # -> ~/Downloads/a-red-square-on-white.png
fgen gen "poster" --out ~/Desktop/poster.png      # exact path; parent dirs are created
fgen gen "variant A" "variant B" --out-dir ./assets --concurrency 2
fgen gen "put them on a beach" --images a.png b.png --out out.png
fgen gen "recolor it blue" --last                 # follow-up edit of the newest generation
fgen gen "keep going" --last 2                    # ...or the second newest
fgen gen "hero image" --json                      # machine-readable JSONL on stdout
fgen last -n 5                                    # list the 5 newest generations (paths)
fgen tui                                          # interactive TUI (same as: fgen -t)
fgen auth                                         # subscription token status
```

Every finished generation is recorded (newest first, capped at 50) in `~/.local/state/fuckinggen/state.json`, which is what `--last` and `fgen last` use. Batch jobs record each image.

Flags that matter:

| Flag | Meaning |
|---|---|
| `-o, --out PATH` | exact file for a single prompt; a directory (or trailing `/`) keeps the derived name |
| `-d, --out-dir DIR` | directory for every job (default `~/Downloads`) |
| `-i, --image PATH` | reference image sent with **every** prompt (repeatable) |
| `--last [N]` | attach the Nth newest generation as a reference (default 1) |
| `-q, --quality` | `low` \| `medium` \| `high` \| `auto` (default `auto`) — the backend may override |
| `-s, --size` | `auto` or `WxH` — a request only; the backend picks the real size |
| `-c, --concurrency` | parallel jobs, 1..=8 (default 1) |
| `--timeout SECS` | per-job deadline (default 240) |
| `--json` | JSONL events on stdout: `start`, `phase`, `saved`, `error`, `summary` |
| `--quiet` | only final paths on stdout |
| `--dry-run` | print the resolved plan, send nothing |

## Agent rules

1. One image per prompt per call. For several assets, pass several prompts (batch) or call once per asset.
2. For follow-up edits, prefer `--last` (or `--last N`) — it attaches the actual previous image, so "make it blue" edits instead of generating something new. `fgen last -n 5` shows what is available.
3. Reference images go in through `--images`; name each one's role in the prompt ("Image 1 is the character to keep").
4. When the user says where to save it, use `--out`/`--out-dir`. Relative paths resolve against the current directory; `~` is expanded; the default is `~/Downloads`.
5. Success evidence is the printed stdout path or the JSON `saved` event. Exit code 1 means at least one job failed (`--json` shows `summary.ok=false`). Never claim an image exists without that evidence.
6. Quality and size are requests. The backend controls final dimensions — if exact pixels matter, inspect the saved file and report what it actually is.
7. Progress goes to stderr (`queued` → `generating` → `finishing`). In pipelines use `--json` or `--quiet`; don't poll the filesystem waiting for a file that hasn't been reported.
8. HTTP 429 rate limits come from the user's own plan — retry later, don't hammer.

## TUI (`fgen -t`)

Interactive terminal UI. Blue accent by default; theme lives in `~/.config/fuckinggen/config.json`.

- Type a prompt, `enter` generates; `shift`+`enter` adds a line and the prompt box grows to fit.
- Drag & drop or paste image paths to attach references — any number of files at once (quoted, backslash-escaped, or `file://` paths all work). Attached refs appear as previews in the `refs` strip above the prompt bar (click one to remove it). A path typed or dropped into the prompt attaches instead of generating, so a drop can never fire a stray prompt.
- **Follow-up edits work in the TUI.** Each prompt carries the currently selected image back to the model, so "change the cup to a cucumber" keeps the girl and edits the cup. `/ctx` toggles that carry (on by default).
- Generated images are **staged in a session cache**, not in your folders: `~/.cache/fuckinggen/session-<pid>-<time>/`. A session starts empty, and anything still unsaved after 24 hours is deleted automatically.
- `enter` with an empty prompt = keep the image you are looking at (moves it to the save directory and records it for `--last`). `/save` and `/save-all` do the same for one/all.
- GALLERY: the session images are a grid of squares below the prompt, plus a `+` cell for a fresh prompt. `↑` enters the grid, `←`/`→` move between cells, `↓` returns to the prompt with that image's prompt loaded; `space` opens the selected picture full screen (`+`/`-` zoom, arrows pan, `space` closes). Cell size follows the number of images. Moving the selection puts that image's prompt back in the prompt bar, so pressing `enter` re-runs it (with the image carried as context) — select, edit, enter is how you iterate.
- While browsing, single keys act on the selected picture: `enter`/`s` keep it · `del` remove it (empty prompt only) · `r` another take (adds "the previous attempt did not land") · `b` transparent cut-out · `y` copy the prompt to the clipboard · `h` history · `+` fresh prompt. `[+]` in the control bar is the same fresh start.
- `/history` lists every kept picture with a thumbnail (enter loads it back into the grid), which is how a session from last week gets iterated on again. The snake high score (`🔥`) is persisted in the config, so it survives restarts.
- First run shows a four-page tour (`/guide` replays it). After 10 generations a one-time three-second coffee popup appears; `enter` opens the link, `esc` skips it, `/settings` row 3 turns it off for good.
- Quitting asks what to keep: `ctrl-c` (or the `[✔ FINISH]` button next to GENERATE) opens a checklist of thumbnails where `space` ticks, `enter` keeps the ticked ones, `n` keeps nothing, and `esc` returns to the work. A second `ctrl-c` while the checklist is open force-quits and leaves the staged files in the cache, printing the path.
- `/help` opens a scrollable guide (`↑↓`/`pgup`/`pgdn` scroll, `esc` closes). The snake restarts on every finished generation and keeps a `🔥` high score in the config.
- While generating, a compact square snake board sits in the middle of the output panel (solid dots, head in the accent colour) and is already listening: arrow keys steer it immediately (no click needed) and crossing an edge wraps around. `Esc` pauses the game.
- The terminal tab shows what the window is doing: the prompt you typed, `⠋ prompt` while it renders, `keep N image(s)?` while the checkpoint is open.
- Type `/` to open the command palette above the prompt bar; `↑`/`↓` pick, `tab` completes, `enter` runs.

Slash commands:

| Command | Does |
|---|---|
| `/open` | open the selected image in the system viewer |
| `/view` | reveal it in the file manager |
| `/save` | keep the selected image (moves it out of the session cache) |
| `/save-all` | keep every image from this session |
| `/new` | empty the prompt for a fresh idea |
| `/regen` | another take on the selected image's prompt |
| `/bg` | same picture, background removed (transparent PNG) |
| `/copy` | copy the selected image's prompt to the clipboard |
| `/guide` | replay the first-run tour |
| `/ctx` | toggle handing the selected image to the next prompt |
| `/root` | move this session's images to `~/Downloads` and save there from now on |
| `/remove` | delete the selected image |
| `/remove-all` | delete every image from this session |
| `/quality low\|medium\|high\|auto` | set generation quality |
| `/dir <path>` | save new images into another folder |
| `/settings` | theme, quality, save dir, "login with codex", refresh auth |
| `/help` | keys and commands |
| `/clear` | clear the prompt |
| `/quit` | leave |

## Troubleshooting

- `no ChatGPT subscription credentials found` → the user runs `codex login` (or `/settings` → "login with codex" in the TUI).
- `HTTP 401` → token stale: `fgen auth`; if still stale, `codex login` again (the CLI refreshes automatically when it can).
- `no finished generation #N to attach yet` → nothing recorded for `--last`; generate once first.
- `the backend finished without an image` → the model declined that prompt; rephrase or simplify.
