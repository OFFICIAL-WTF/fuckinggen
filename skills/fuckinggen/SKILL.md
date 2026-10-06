---
name: fuckinggen
description: Generate or edit images with the user's ChatGPT subscription from the terminal via the fgen/fuckinggen CLI (Codex backend) - text-to-image, reference-image edits, batch jobs, live progress, and an interactive TUI. Use for any request to create or modify raster images (illustrations, photos, posters, mockups, product shots, assets).
---

# fuckinggen

Terminal image generation on the user's **own ChatGPT subscription** — no API key, no per-image API billing. It calls the same ChatGPT/Codex backend channel the Codex CLI uses, with the login already on this machine. Today's model is **ChatGPT Images 2.5**.

Binary: `fgen` (alias: `fuckinggen`). Source: `~/GSpace/Opensource/HTF/fuckinggen` (GitHub: `prophesourvolodymyr/fuckinggen`).

## When to use

- The user asks for an image: illustration, photo, poster, mockup, texture, sprite, character, product shot, logo concept (raster), meme, diagram art.
- The user asks to modify an existing image ("make it blue", "add a hat", "put both characters together").
- Anything that should end up as a PNG file. For SVG/vector or code-native UI assets, use the project's own design system instead.

## Prerequisites

1. `fgen` on PATH (`which fgen`). If missing: `cd ~/GSpace/Opensource/HTF/fuckinggen && cargo install --path .`
2. Subscription login: `fgen auth` must report a valid token. If it fails, ask the user to run `codex login`, then `fgen auth` again. **Never** ask for or use an OpenAI API key.

## Commands

Generate (default save dir `~/Downloads`; the filename comes from the prompt; existing files are never overwritten — `-v2`, `-v3`, … are used instead):

```bash
fgen gen "a red square on white"                  # -> ~/Downloads/a-red-square-on-white.png
fgen gen "poster" --out ~/Desktop/poster.png      # exact path; parent dirs are created
fgen gen "variant A" "variant B" --out-dir ./assets --concurrency 2
fgen gen "put them on a beach" --images a.png b.png --out out.png
fgen gen "hero image" --json                      # machine-readable JSONL on stdout
fgen tui                                          # interactive TUI (same as: fgen -t)
```

Flags that matter:

| Flag | Meaning |
|---|---|
| `-o, --out PATH` | exact file for a single prompt; a directory (or trailing `/`) keeps the derived name |
| `-d, --out-dir DIR` | directory for every job (default `~/Downloads`) |
| `-i, --image PATH` | reference image sent with **every** prompt (repeatable) |
| `-q, --quality` | `low` \| `medium` \| `high` \| `auto` (default `auto`) — the backend may override |
| `-s, --size` | `auto` or `WxH` — a request only; the backend picks the real size |
| `-c, --concurrency` | parallel jobs, 1..=8 (default 1) |
| `--timeout SECS` | per-job deadline (default 240) |
| `--json` | JSONL events on stdout: `start`, `phase`, `saved`, `error`, `summary` |
| `--quiet` | only final paths on stdout |
| `--dry-run` | print the resolved plan, send nothing |

## Agent rules

1. One image per prompt per call. For several assets, pass several prompts (batch) or call once per asset.
2. Reference images go in through `--images`; name each one's role in the prompt ("Image 1 is the character to keep").
3. When the user says where to save it, use `--out`/`--out-dir`. Relative paths resolve against the current directory; `~` is expanded; the default is `~/Downloads`.
4. Success evidence is the printed stdout path or the JSON `saved` event. Exit code 1 means at least one job failed (`--json` shows `summary.ok=false`). Never claim an image exists without that evidence.
5. Quality and size are requests. The backend controls final dimensions — if exact pixels matter, inspect the saved file and report what it actually is.
6. Progress goes to stderr (`queued` → `generating` → `finishing`). In pipelines use `--json` or `--quiet`; don't poll the filesystem waiting for a file that hasn't been reported.
7. HTTP 429 rate limits come from the user's own plan — retry later, don't hammer.

## TUI (`fgen -t`)

Interactive terminal UI: type a prompt, press Enter. Drag & drop or paste an image path to attach a reference. Follow-up prompts edit the last generated image. `tab` cycles quality, `esc` cancels/clears, `ctrl-c` quits. Images render inline in the terminal and save into the current directory.

## Troubleshooting

- `no ChatGPT subscription credentials found` → the user runs `codex login`.
- `HTTP 401` → token stale: `fgen auth`; if still stale, `codex login` again (the CLI refreshes automatically when it can).
- `the backend finished without an image` → the model declined that prompt; rephrase or simplify.
