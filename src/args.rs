use anyhow::{Result, anyhow, bail};

#[derive(Debug, Clone, PartialEq)]
pub struct GenArgs {
    pub prompts: Vec<String>,
    pub out: Option<String>,
    pub out_dir: Option<String>,
    pub images: Vec<String>,
    /// Attach the Nth most recent generation as a reference (1 = latest).
    pub last: Option<usize>,
    pub quality: String,
    pub size: Option<String>,
    pub model: String,
    pub image_model: Option<String>,
    pub concurrency: usize,
    pub timeout_secs: u64,
    pub json: bool,
    pub quiet: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Gen(Box<GenArgs>),
    Tui(Box<TuiArgs>),
    Last { count: usize, json: bool },
    Auth { json: bool },
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiArgs {
    pub initial_prompt: Option<String>,
    pub out_dir: Option<String>,
}

pub const HELP: &str = "\
fuckinggen — generate images with your ChatGPT subscription

Usage:
  fuckinggen gen [PROMPT...] [options]
  fuckinggen tui [PROMPT...] [-d DIR]    (same as: fuckinggen -t, fgen -t)
  fuckinggen last [-n N] [--json]         recent generations, newest first
  fuckinggen auth [--json]
  fuckinggen help | --version

gen options:
  -p, --prompt <text>       add a prompt (positional prompts also accepted)
  -o, --out <path>          save path for a single prompt; a directory or
                            trailing slash keeps the prompt-derived filename
  -d, --out-dir <dir>       save directory (default: $FUCKINGGEN_OUT_DIR or ~/Downloads)
  -i, --image <path>        reference image sent with every prompt (repeatable)
      --last [N]            also attach the Nth most recent generation (default 1)
  -q, --quality <q>         low | medium | high | auto (default: auto)
  -s, --size <auto|WxH>     ask for a size (the backend may override it)
  -m, --model <slug>        carrier model (default: gpt-6-sol)
      --image-model <slug>  pin the image model for the generation tool
  -c, --concurrency <n>     parallel jobs (default: 1, max: 8)
      --timeout <secs>      per-job deadline (default: 240, max: 1800)
      --json                JSONL events on stdout, machine-readable
      --quiet               only final paths on stdout
      --dry-run             resolve paths and print the plan, send nothing

Output:
  stdout gets absolute saved paths, one per line. Progress goes to stderr.
  Existing files are never overwritten: -v2, -v3, ... is used instead.
";

pub fn parse(argv: &[String]) -> Result<Command> {
    let Some(first) = argv.first() else {
        return Ok(Command::Help);
    };
    match first.as_str() {
        "gen" | "generate" | "image" => parse_gen(&argv[1..]),
        "auth" | "status" => {
            let mut json = false;
            for arg in &argv[1..] {
                match arg.as_str() {
                    "--json" => json = true,
                    "-h" | "--help" => return Ok(Command::Help),
                    other => bail!("unknown auth option: {other}"),
                }
            }
            Ok(Command::Auth { json })
        }
        "tui" | "-t" | "--tui" => parse_tui(&argv[1..]),
        "last" => parse_last(&argv[1..]),
        "-h" | "--help" | "help" => Ok(Command::Help),
        "-V" | "--version" | "version" => Ok(Command::Version),
        other => bail!("unknown command: {other}\n\n{HELP}"),
    }
}

fn parse_last(args: &[String]) -> Result<Command> {
    let mut count = 10usize;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" => return Ok(Command::Help),
            "-n" | "--count" => {
                let value = take(args, &mut i, arg)?;
                count = value
                    .parse()
                    .map_err(|_| anyhow!("--count must be a number (got {value})"))?;
                if count == 0 {
                    bail!("--count must be at least 1");
                }
            }
            "--json" => json = true,
            other => bail!("unknown last option: {other}\n\n{HELP}"),
        }
        i += 1;
    }
    Ok(Command::Last { count, json })
}

fn parse_tui(args: &[String]) -> Result<Command> {
    let mut out_dir = None;
    let mut prompt_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" => return Ok(Command::Help),
            "-d" | "--out-dir" => out_dir = Some(take(args, &mut i, arg)?),
            "--" => {
                i += 1;
                while i < args.len() {
                    prompt_parts.push(args[i].clone());
                    i += 1;
                }
                break;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                bail!("unknown tui option: {other}\n\n{HELP}")
            }
            other => prompt_parts.push(other.to_string()),
        }
        i += 1;
    }
    let initial_prompt = if prompt_parts.is_empty() {
        None
    } else {
        Some(prompt_parts.join(" "))
    };
    Ok(Command::Tui(Box::new(TuiArgs {
        initial_prompt,
        out_dir,
    })))
}

fn parse_gen(args: &[String]) -> Result<Command> {
    let mut g = GenArgs {
        prompts: Vec::new(),
        out: None,
        out_dir: None,
        images: Vec::new(),
        last: None,
        quality: "auto".to_string(),
        size: None,
        model: crate::api::DEFAULT_MODEL.to_string(),
        image_model: None,
        concurrency: 1,
        timeout_secs: 240,
        json: false,
        quiet: false,
        dry_run: false,
    };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" => return Ok(Command::Help),
            "-p" | "--prompt" => g.prompts.push(take(args, &mut i, arg)?),
            "-o" | "--out" => g.out = Some(take(args, &mut i, arg)?),
            "-d" | "--out-dir" => g.out_dir = Some(take(args, &mut i, arg)?),
            "-i" | "--image" | "--images" => g.images.push(take(args, &mut i, arg)?),
            "--last" => {
                let mut count = 1usize;
                if let Some(next) = args.get(i + 1)
                    && let Ok(parsed) = next.parse::<usize>()
                {
                    if parsed == 0 {
                        bail!("--last index starts at 1");
                    }
                    count = parsed;
                    i += 1;
                }
                g.last = Some(count);
            }
            "-q" | "--quality" => {
                let value = take(args, &mut i, arg)?;
                if !matches!(value.as_str(), "low" | "medium" | "high" | "auto") {
                    bail!("--quality must be low, medium, high, or auto (got {value})");
                }
                g.quality = value;
            }
            "-s" | "--size" => {
                let value = take(args, &mut i, arg)?;
                validate_size(&value)?;
                g.size = Some(value);
            }
            "-m" | "--model" => g.model = take(args, &mut i, arg)?,
            "--image-model" => g.image_model = Some(take(args, &mut i, arg)?),
            "-c" | "--concurrency" => {
                let value = take(args, &mut i, arg)?;
                let n: usize = value
                    .parse()
                    .map_err(|_| anyhow!("--concurrency must be a number (got {value})"))?;
                if !(1..=8).contains(&n) {
                    bail!("--concurrency must be 1..=8 (got {n})");
                }
                g.concurrency = n;
            }
            "--timeout" => {
                let value = take(args, &mut i, arg)?;
                let n: u64 = value
                    .parse()
                    .map_err(|_| anyhow!("--timeout must be a number of seconds (got {value})"))?;
                if n == 0 || n > 1800 {
                    bail!("--timeout must be 1..=1800 seconds (got {n})");
                }
                g.timeout_secs = n;
            }
            "--json" => g.json = true,
            "--quiet" => g.quiet = true,
            "--dry-run" => g.dry_run = true,
            "--" => {
                i += 1;
                while i < args.len() {
                    g.prompts.push(args[i].clone());
                    i += 1;
                }
                break;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                bail!("unknown option: {other}\n\n{HELP}")
            }
            other => g.prompts.push(other.to_string()),
        }
        i += 1;
    }
    if g.prompts.is_empty() {
        bail!("nothing to generate — pass at least one prompt\n\n{HELP}");
    }
    if g.out.is_some() && g.prompts.len() > 1 {
        bail!("--out takes a single path; use --out-dir for multiple prompts");
    }
    if g.out.is_some() && g.out_dir.is_some() {
        bail!("use either --out or --out-dir, not both");
    }
    if g.quiet && g.json {
        bail!("--quiet and --json are mutually exclusive");
    }
    Ok(Command::Gen(Box::new(g)))
}

fn take(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| anyhow!("{flag} needs a value"))
}

pub fn validate_size(value: &str) -> Result<()> {
    if value == "auto" {
        return Ok(());
    }
    let split = value.split_once('x').or_else(|| value.split_once('X'));
    let Some((w, h)) = split else {
        bail!("--size must be auto or WxH (e.g. 1024x1536)");
    };
    let w: u32 = w
        .parse()
        .map_err(|_| anyhow!("--size width must be a number (got {w})"))?;
    let h: u32 = h
        .parse()
        .map_err(|_| anyhow!("--size height must be a number (got {h})"))?;
    for edge in [w, h] {
        if edge == 0 || edge % 16 != 0 || edge > 3840 {
            bail!("--size edges must be multiples of 16 in 16..=3840 (got {w}x{h})");
        }
    }
    let (long, short) = (w.max(h), w.min(h));
    if long > short.saturating_mul(3) {
        bail!("--size ratio must be at most 3:1 (got {w}x{h})");
    }
    let pixels = w as u64 * h as u64;
    if !(655_360..=8_294_400).contains(&pixels) {
        bail!("--size total pixels must be 655360..=8294400 (got {pixels})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_basic_gen() {
        let cmd = parse(&args(&[
            "gen",
            "a red square",
            "--out",
            "x.png",
            "--quality",
            "low",
        ]))
        .unwrap();
        let Command::Gen(g) = cmd else {
            panic!("expected gen")
        };
        assert_eq!(g.prompts, vec!["a red square"]);
        assert_eq!(g.out.as_deref(), Some("x.png"));
        assert_eq!(g.quality, "low");
        assert_eq!(g.model, crate::api::DEFAULT_MODEL);
    }

    #[test]
    fn positional_and_flag_prompts_combine() {
        let cmd = parse(&args(&[
            "gen", "first", "--prompt", "second", "-p", "third",
        ]))
        .unwrap();
        let Command::Gen(g) = cmd else {
            panic!("expected gen")
        };
        assert_eq!(g.prompts, vec!["first", "second", "third"]);
    }

    #[test]
    fn double_dash_keeps_dashes_in_prompts() {
        let cmd = parse(&args(&["gen", "--", "--not-a-flag"])).unwrap();
        let Command::Gen(g) = cmd else {
            panic!("expected gen")
        };
        assert_eq!(g.prompts, vec!["--not-a-flag"]);
    }

    #[test]
    fn parses_last_flag_and_command() {
        let cmd = parse(&args(&["gen", "x", "--last"])).unwrap();
        let Command::Gen(g) = cmd else {
            panic!("expected gen")
        };
        assert_eq!(g.last, Some(1));

        let cmd = parse(&args(&["gen", "x", "--last", "3"])).unwrap();
        let Command::Gen(g) = cmd else {
            panic!("expected gen")
        };
        assert_eq!(g.last, Some(3));

        let cmd = parse(&args(&["last", "-n", "5", "--json"])).unwrap();
        let Command::Last { count, json } = cmd else {
            panic!("expected last")
        };
        assert_eq!(count, 5);
        assert!(json);

        assert!(parse(&args(&["gen", "x", "--last", "0"])).is_err());
    }

    #[test]
    fn parses_tui_invocations() {
        for argv in [vec!["tui"], vec!["-t"], vec!["--tui"]] {
            let cmd = parse(&args(&argv)).unwrap();
            assert!(matches!(cmd, Command::Tui(_)));
        }
        let cmd = parse(&args(&["-t", "make a logo", "-d", "/tmp/x"])).unwrap();
        let Command::Tui(t) = cmd else {
            panic!("expected tui")
        };
        assert_eq!(t.initial_prompt.as_deref(), Some("make a logo"));
        assert_eq!(t.out_dir.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn rejects_conflicting_output_flags() {
        assert!(parse(&args(&["gen", "a", "b", "--out", "x.png"])).is_err());
        assert!(parse(&args(&["gen", "a", "--out", "x.png", "--out-dir", "d"])).is_err());
    }

    #[test]
    fn rejects_bad_inputs() {
        assert!(parse(&args(&["gen"])).is_err());
        assert!(parse(&args(&["gen", "a", "--quality", "ultra"])).is_err());
        assert!(parse(&args(&["gen", "a", "--size", "1000x1000"])).is_err());
        assert!(parse(&args(&["gen", "a", "--concurrency", "0"])).is_err());
        assert!(parse(&args(&["gen", "a", "--nope"])).is_err());
        assert!(parse(&args(&["gen", "a", "--json", "--quiet"])).is_err());
        assert!(parse(&args(&["nonsense"])).is_err());
    }

    #[test]
    fn size_validation_rules() {
        assert!(validate_size("auto").is_ok());
        assert!(validate_size("1024x1024").is_ok());
        assert!(validate_size("1536x1024").is_ok());
        assert!(validate_size("2048x2048").is_ok());
        assert!(validate_size("1000x1000").is_err());
        assert!(validate_size("3840x3840").is_err());
        assert!(validate_size("4096x1024").is_err());
        assert!(validate_size("1024x256").is_err());
    }
}
