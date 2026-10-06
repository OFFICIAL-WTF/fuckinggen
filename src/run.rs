use anyhow::{Context, Result};
use serde_json::json;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::api::{self, Generated, Phase, Spec};
use crate::args::{Command, GenArgs};
use crate::auth;
use crate::files;
use crate::images;

pub fn run(cmd: Command) -> Result<i32> {
    match cmd {
        Command::Help => {
            print!("{}", crate::args::HELP);
            Ok(0)
        }
        Command::Version => {
            println!("fuckinggen {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Command::Auth { json } => run_auth(json),
        Command::Tui(args) => crate::tui::run_tui(*args),
        Command::Gen(args) => run_gen(*args),
    }
}

fn run_auth(json: bool) -> Result<i32> {
    let cred = auth::load(None)?;
    let now = files::now_unix();
    let info = json!({
        "source": cred.path.display().to_string(),
        "kind": auth::kind_name(cred.kind),
        "account_id": cred.account_id,
        "expires_at": cred.expires_at.map(files::rfc3339),
        "expires_in_secs": cred.expires_at.map(|at| at - now),
        "valid": cred.valid(),
        "refresh_token": cred.refresh.is_some(),
    });
    if json {
        println!("{}", serde_json::to_string(&info)?);
    } else {
        println!("source:  {}", cred.path.display());
        println!("kind:    {}", auth::kind_name(cred.kind));
        println!("account: {}", cred.masked_account());
        match cred.expires_at {
            Some(at) if cred.valid() => {
                println!("expires: {} (in {}s)", files::rfc3339(at), at - now)
            }
            Some(at) => println!(
                "expires: {} (EXPIRED {}s ago — run `codex login` if refresh fails)",
                files::rfc3339(at),
                now - at
            ),
            None => println!("expires: unknown"),
        }
        println!(
            "refresh: {}",
            if cred.refresh.is_some() {
                "present"
            } else {
                "missing"
            }
        );
    }
    Ok(0)
}

struct JobPlan {
    prompt: String,
    target: PathBuf,
}

enum Msg {
    Start {
        index: usize,
        total: usize,
        prompt: String,
        target: PathBuf,
    },
    Phase {
        index: usize,
        total: usize,
        phase: Phase,
        elapsed: Duration,
    },
    Saved {
        index: usize,
        total: usize,
        path: PathBuf,
        bytes: usize,
        elapsed: Duration,
    },
    Failed {
        index: usize,
        total: usize,
        error: String,
        elapsed: Duration,
    },
}

fn run_gen(args: GenArgs) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let out_dir = args
        .out_dir
        .as_deref()
        .map(|dir| PathBuf::from(files::expand_tilde(dir)));
    let ref_paths: Vec<PathBuf> = args
        .images
        .iter()
        .map(|path| PathBuf::from(files::expand_tilde(path)))
        .collect();
    let jobs: Vec<JobPlan> = args
        .prompts
        .iter()
        .map(|prompt| JobPlan {
            prompt: prompt.clone(),
            target: files::resolve_out_path(args.out.as_deref(), out_dir.as_deref(), prompt, &cwd),
        })
        .collect();

    if args.dry_run {
        for job in &jobs {
            let report = json!({
                "prompt": job.prompt,
                "out": job.target.display().to_string(),
                "model": args.model,
                "quality": args.quality,
                "size": args.size,
                "image_model": args.image_model,
                "reference_images": ref_paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        return Ok(0);
    }

    let initial = auth::load(None)?;
    let refs: Vec<String> = ref_paths
        .iter()
        .map(|path| images::to_data_url(path))
        .collect::<Result<_>>()?;

    let printer = Printer::new(args.json, args.quiet);
    let total = jobs.len();
    let agent = crate::http::agent();
    let timeout = Duration::from_secs(args.timeout_secs);
    let spec = Spec {
        prompt: String::new(),
        quality: args.quality.clone(),
        size: args.size.clone(),
        model: args.model.clone(),
        image_model: args.image_model.clone(),
    };
    let concurrency = args.concurrency.min(total);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<Msg>();

    let mut saved_paths: Vec<PathBuf> = Vec::new();
    let mut failures: Vec<(usize, String)> = Vec::new();
    let started = Instant::now();

    std::thread::scope(|scope| {
        for _ in 0..concurrency {
            let agent = agent.clone();
            let tx = tx.clone();
            let jobs = &jobs;
            let refs = &refs;
            let spec = &spec;
            let next = &next;
            let auth_path = initial.path.clone();
            let account_id = initial.account_id.clone();
            let token = initial.access.clone();
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(job) = jobs.get(index) else { break };
                    let _ = tx.send(Msg::Start {
                        index,
                        total,
                        prompt: job.prompt.clone(),
                        target: job.target.clone(),
                    });
                    let t0 = Instant::now();
                    let mut local_spec = spec.clone();
                    local_spec.prompt = job.prompt.clone();
                    let body = api::build_request(&local_spec, refs);
                    let mut current_token = token.clone();
                    let mut attempt = 0;
                    loop {
                        attempt += 1;
                        let mut on_phase = |phase: Phase| {
                            let _ = tx.send(Msg::Phase {
                                index,
                                total,
                                phase,
                                elapsed: t0.elapsed(),
                            });
                        };
                        match api::run(&agent, &current_token, account_id.as_deref(), &body, timeout, &mut on_phase) {
                            Ok(generated) => {
                                match write_output(&job.target, &generated) {
                                    Ok(path) => {
                                        let _ = tx.send(Msg::Saved {
                                            index,
                                            total,
                                            path,
                                            bytes: generated.bytes.len(),
                                            elapsed: t0.elapsed(),
                                        });
                                    }
                                    Err(err) => {
                                        let _ = tx.send(Msg::Failed {
                                            index,
                                            total,
                                            error: format!("{err:#}"),
                                            elapsed: t0.elapsed(),
                                        });
                                    }
                                }
                                break;
                            }
                            Err(err) => {
                                let message = format!("{err:#}");
                                if attempt == 1 && message.contains("HTTP 401") {
                                    match auth::load_from(&auth_path)
                                        .and_then(|cred| auth::refresh(&agent, &cred))
                                    {
                                        Ok(fresh) => {
                                            current_token = fresh.access;
                                            continue;
                                        }
                                        Err(refresh_error) => {
                                            let _ = tx.send(Msg::Failed {
                                                index,
                                                total,
                                                error: format!("{message}; token refresh failed: {refresh_error:#}"),
                                                elapsed: t0.elapsed(),
                                            });
                                            break;
                                        }
                                    }
                                }
                                let _ = tx.send(Msg::Failed {
                                    index,
                                    total,
                                    error: message,
                                    elapsed: t0.elapsed(),
                                });
                                break;
                            }
                        }
                    }
                }
            });
        }
        drop(tx);
        for msg in rx {
            match msg {
                Msg::Start {
                    index,
                    total,
                    prompt,
                    target,
                } => printer.start(index, total, &prompt, &target),
                Msg::Phase {
                    index,
                    total,
                    phase,
                    elapsed,
                } => printer.phase(index, total, &phase, elapsed),
                Msg::Saved {
                    index,
                    total,
                    path,
                    bytes,
                    elapsed,
                } => {
                    printer.saved(index, total, &path, bytes, elapsed);
                    saved_paths.push(path);
                }
                Msg::Failed {
                    index,
                    total,
                    error,
                    elapsed,
                } => {
                    printer.failed(index, total, &error, elapsed);
                    failures.push((index, error));
                }
            }
        }
    });

    let elapsed = started.elapsed();
    printer.summary(failures.is_empty(), elapsed, &saved_paths, &failures);
    Ok(if failures.is_empty() { 0 } else { 1 })
}

fn write_output(target: &Path, generated: &Generated) -> Result<PathBuf> {
    let path = files::prepare_path(target)?;
    files::atomic_write(&path, &generated.bytes)?;
    Ok(path)
}

struct Printer {
    json: bool,
    quiet: bool,
    tty: bool,
}

impl Printer {
    fn new(json: bool, quiet: bool) -> Self {
        Printer {
            json,
            quiet,
            tty: std::io::stderr().is_terminal(),
        }
    }

    fn start(&self, index: usize, total: usize, prompt: &str, target: &Path) {
        if self.json {
            self.json_line(&json!({
                "event": "start",
                "job": index,
                "total": total,
                "prompt": prompt,
                "out": target.display().to_string(),
            }));
            return;
        }
        if self.quiet {
            return;
        }
        let label = format!(
            "[job {}/{}] {}",
            index + 1,
            total,
            auth::truncate(prompt, 60)
        );
        if self.tty {
            let _ = write!(std::io::stderr(), "{label} ...");
            let _ = std::io::stderr().flush();
        } else {
            eprintln!("{label}");
        }
    }

    fn phase(&self, index: usize, total: usize, phase: &Phase, elapsed: Duration) {
        if self.json {
            self.json_line(&json!({
                "event": "phase",
                "job": index,
                "total": total,
                "phase": phase.as_str(),
                "elapsed_ms": elapsed.as_millis() as u64,
            }));
            return;
        }
        if self.quiet {
            return;
        }
        let line = format!(
            "[job {}/{}] {} ({:.1}s)",
            index + 1,
            total,
            phase.as_str(),
            elapsed.as_secs_f64()
        );
        if self.tty {
            let _ = write!(std::io::stderr(), "\r\x1b[K{line}");
            let _ = std::io::stderr().flush();
        } else {
            eprintln!("{line}");
        }
    }

    fn saved(&self, index: usize, total: usize, path: &Path, bytes: usize, elapsed: Duration) {
        if self.json {
            self.json_line(&json!({
                "event": "saved",
                "job": index,
                "total": total,
                "path": path.display().to_string(),
                "bytes": bytes,
                "elapsed_ms": elapsed.as_millis() as u64,
            }));
            return;
        }
        if self.tty {
            let _ = write!(std::io::stderr(), "\r\x1b[K");
        }
        if !self.quiet {
            eprintln!(
                "[job {}/{}] saved {} ({:.1}s, {})",
                index + 1,
                total,
                path.display(),
                elapsed.as_secs_f64(),
                human_bytes(bytes)
            );
        }
        println!("{}", path.display());
        let _ = std::io::stdout().flush();
    }

    fn failed(&self, index: usize, total: usize, error: &str, elapsed: Duration) {
        if self.json {
            self.json_line(&json!({
                "event": "error",
                "job": index,
                "total": total,
                "error": error,
                "elapsed_ms": elapsed.as_millis() as u64,
            }));
            return;
        }
        if self.tty {
            let _ = write!(std::io::stderr(), "\r\x1b[K");
        }
        eprintln!(
            "[job {}/{}] failed after {:.1}s: {}",
            index + 1,
            total,
            elapsed.as_secs_f64(),
            error
        );
    }

    fn summary(
        &self,
        ok: bool,
        elapsed: Duration,
        saved: &[PathBuf],
        failures: &[(usize, String)],
    ) {
        if self.json {
            self.json_line(&json!({
                "event": "summary",
                "ok": ok,
                "elapsed_ms": elapsed.as_millis() as u64,
                "saved": saved.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "failed": failures
                    .iter()
                    .map(|(job, error)| json!({"job": job, "error": error}))
                    .collect::<Vec<_>>(),
            }));
            return;
        }
        if self.quiet {
            return;
        }
        eprintln!(
            "done in {:.1}s — {} saved, {} failed",
            elapsed.as_secs_f64(),
            saved.len(),
            failures.len()
        );
    }

    fn json_line(&self, value: &serde_json::Value) {
        println!(
            "{}",
            serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
        );
        let _ = std::io::stdout().flush();
    }
}

fn human_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
