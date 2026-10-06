use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::api::{self, Phase, Spec};
use crate::args::TuiArgs;
use crate::{auth, files, http, images};

const TICK: Duration = Duration::from_millis(80);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENTS: [Color; 5] = [
    Color::Cyan,
    Color::LightCyan,
    Color::Blue,
    Color::LightBlue,
    Color::Cyan,
];
const QUALITIES: [&str; 4] = ["low", "medium", "high", "auto"];

enum Job {
    Idle,
    Running {
        cancel: Arc<AtomicBool>,
        rx: mpsc::Receiver<WorkerMsg>,
        started: Instant,
        phase: Phase,
    },
}

enum WorkerMsg {
    Phase(Phase),
    Done(Box<Done>),
    Failed(String),
}

struct Done {
    path: PathBuf,
    bytes: usize,
    elapsed: Duration,
    image: image::DynamicImage,
}

#[derive(Clone, Copy)]
enum Level {
    Info,
    Ok,
    Err,
}

struct App {
    picker: Picker,
    input: String,
    cursor: usize,
    refs: Vec<PathBuf>,
    last_image_path: Option<PathBuf>,
    protocol: Option<StatefulProtocol>,
    job: Job,
    tick: u64,
    status: String,
    level: Level,
    out_dir: PathBuf,
    auth_note: String,
    quality: String,
    quit: bool,
}

impl App {
    fn new(args: TuiArgs) -> Result<Self> {
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        let auth_note = match auth::load(None) {
            Ok(cred) => format!(
                "chatgpt subscription · {} · {}",
                auth::kind_name(cred.kind),
                cred.masked_account()
            ),
            Err(err) => format!(
                "auth problem: {}",
                crate::api::snippet(&format!("{err:#}"), 80)
            ),
        };
        let out_dir = match args.out_dir.as_deref() {
            Some(dir) => PathBuf::from(files::expand_tilde(dir)),
            None => std::env::current_dir().context("reading current directory")?,
        };
        let input = args.initial_prompt.unwrap_or_default();
        let cursor = input.chars().count();
        Ok(App {
            picker,
            input,
            cursor,
            refs: Vec::new(),
            last_image_path: None,
            protocol: None,
            job: Job::Idle,
            tick: 0,
            status: format!("saving to {}", out_dir.display()),
            level: Level::Info,
            out_dir,
            auth_note,
            quality: "auto".to_string(),
            quit: false,
        })
    }

    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| draw(frame, self))?;
            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(key) => self.on_key(key),
                    Event::Paste(text) => self.on_paste(&text),
                    _ => {}
                }
            }
            self.tick += 1;
            self.drain_worker();
        }
        Ok(())
    }

    fn set_status(&mut self, level: Level, text: impl Into<String>) {
        self.level = level;
        self.status = text.into();
    }

    fn on_paste(&mut self, text: &str) {
        if let Some(path) = paste_reference(text) {
            if !self.refs.contains(&path) {
                self.refs.push(path.clone());
            }
            self.set_status(Level::Info, format!("attached {}", display_name(&path)));
            return;
        }
        insert_text(&mut self.input, &mut self.cursor, text);
    }

    fn on_key(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('q'), KeyModifiers::CONTROL) => self.quit = true,
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.input.clear();
                self.cursor = 0;
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                word_delete(&mut self.input, &mut self.cursor)
            }
            (KeyCode::Esc, _) => {
                if let Job::Running { cancel, .. } = &self.job {
                    cancel.store(true, Ordering::Relaxed);
                    self.set_status(Level::Info, "cancelling…");
                } else {
                    self.input.clear();
                    self.cursor = 0;
                }
            }
            (KeyCode::Enter, KeyModifiers::SHIFT) => {
                insert_text(&mut self.input, &mut self.cursor, "\n")
            }
            (KeyCode::Enter, _) => self.submit(),
            (KeyCode::Backspace, _) => backspace(&mut self.input, &mut self.cursor),
            (KeyCode::Delete, _) => delete_forward(&mut self.input, &mut self.cursor),
            (KeyCode::Left, _) => self.cursor = self.cursor.saturating_sub(1),
            (KeyCode::Right, _) => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            (KeyCode::Home, _) | (KeyCode::PageUp, _) => self.cursor = 0,
            (KeyCode::End, _) | (KeyCode::PageDown, _) => self.cursor = self.input.chars().count(),
            (KeyCode::Up, _) => {
                if let Some(last) = self.last_image_path.as_ref()
                    && let Some(name) = last.file_stem().and_then(|s| s.to_str())
                {
                    self.set_status(Level::Info, format!("editing {name}"));
                }
            }
            (KeyCode::Tab, _) => {
                let index = QUALITIES
                    .iter()
                    .position(|q| *q == self.quality.as_str())
                    .unwrap_or(QUALITIES.len() - 1);
                self.quality = QUALITIES[(index + 1) % QUALITIES.len()].to_string();
                let quality = self.quality.clone();
                self.set_status(Level::Info, format!("quality {quality}"));
            }
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                insert_text(&mut self.input, &mut self.cursor, &c.to_string())
            }
            _ => {}
        }
    }

    fn submit(&mut self) {
        if matches!(self.job, Job::Running { .. }) {
            return;
        }
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let mut ref_paths = std::mem::take(&mut self.refs);
        if let Some(last) = &self.last_image_path {
            ref_paths.push(last.clone());
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        spawn_worker(
            prompt,
            ref_paths,
            self.out_dir.clone(),
            self.quality.clone(),
            tx,
            cancel.clone(),
        );
        self.job = Job::Running {
            cancel,
            rx,
            started: Instant::now(),
            phase: Phase::Queued,
        };
        self.input.clear();
        self.cursor = 0;
        self.set_status(Level::Info, "generating…");
    }

    fn drain_worker(&mut self) {
        let Job::Running {
            rx, started, phase, ..
        } = &mut self.job
        else {
            return;
        };
        let elapsed = started.elapsed();
        loop {
            match rx.try_recv() {
                Ok(WorkerMsg::Phase(next)) => *phase = next,
                Ok(WorkerMsg::Done(done)) => {
                    let protocol = self.picker.new_resize_protocol(done.image.clone());
                    self.protocol = Some(protocol);
                    self.last_image_path = Some(done.path.clone());
                    self.set_status(
                        Level::Ok,
                        format!(
                            "saved {} ({:.1}s, {})",
                            done.path.display(),
                            done.elapsed.as_secs_f64(),
                            human_bytes(done.bytes)
                        ),
                    );
                    self.job = Job::Idle;
                    return;
                }
                Ok(WorkerMsg::Failed(err)) => {
                    self.set_status(Level::Err, truncate_chars(&err, 220));
                    self.job = Job::Idle;
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.set_status(Level::Err, "generation worker vanished");
                    self.job = Job::Idle;
                    return;
                }
            }
        }
        let _ = elapsed;
    }

    fn display_input(&self) -> String {
        let chars: Vec<char> = self.input.chars().collect();
        let cursor = self.cursor.min(chars.len());
        let mut out = String::new();
        for (index, ch) in chars.iter().enumerate() {
            if index == cursor {
                out.push('▏');
            }
            out.push(*ch);
        }
        if cursor == chars.len() {
            out.push('▏');
        }
        out
    }
}

fn spawn_worker(
    prompt: String,
    ref_paths: Vec<PathBuf>,
    out_dir: PathBuf,
    quality: String,
    tx: mpsc::Sender<WorkerMsg>,
    cancel: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let phase_tx = tx.clone();
        let result = (|| -> Result<Done> {
            let cred = auth::load(None)?;
            let agent = http::agent();
            let mut data_urls = Vec::new();
            for path in &ref_paths {
                data_urls.push(images::to_data_url(path)?);
            }
            let spec = Spec {
                prompt: prompt.clone(),
                quality,
                size: None,
                model: api::DEFAULT_MODEL.to_string(),
                image_model: None,
            };
            let body = api::build_request(&spec, &data_urls);
            let started = Instant::now();
            let mut on_phase = |phase: Phase| {
                let _ = phase_tx.send(WorkerMsg::Phase(phase));
            };
            let generated = api::run_cancellable(
                &agent,
                &cred.access,
                cred.account_id.as_deref(),
                &body,
                Duration::from_secs(600),
                &cancel,
                &mut on_phase,
            )?;
            let target = files::resolve_out_path(None, Some(&out_dir), &prompt, &out_dir);
            let path = files::prepare_path(&target)?;
            files::atomic_write(&path, &generated.bytes)?;
            let image = image::load_from_memory(&generated.bytes)
                .context("decoding the generated image")?;
            Ok(Done {
                path,
                bytes: generated.bytes.len(),
                elapsed: started.elapsed(),
                image,
            })
        })();
        let message = match result {
            Ok(done) => WorkerMsg::Done(Box::new(done)),
            Err(err) => WorkerMsg::Failed(format!("{err:#}")),
        };
        let _ = tx.send(message);
    });
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let inner_width = area.width.saturating_sub(4).max(8);
    let input_lines = wrapped_lines(&app.display_input(), inner_width).clamp(1, 6) as u16;
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(input_lines + 2),
        Constraint::Length(1),
    ])
    .split(area);

    draw_header(frame, app, chunks[0]);
    draw_stage(frame, app, chunks[1]);
    draw_input(frame, app, chunks[2]);
    draw_status(frame, app, chunks[3]);
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let left = Line::from(vec![
        Span::styled(
            " fuckinggen ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(app.auth_note.clone(), Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(left), area);
    let right = Line::from(Span::styled(
        format!("quality: {} ", app.quality),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

fn draw_stage(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let title = app
        .last_image_path
        .as_ref()
        .and_then(|path| path.file_name().and_then(|name| name.to_str()))
        .map(|name| format!(" output · {name} "))
        .unwrap_or_else(|| " output ".to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(
            Style::default().fg(if matches!(app.job, Job::Running { .. }) {
                ACCENTS[((app.tick / 3) % ACCENTS.len() as u64) as usize]
            } else {
                Color::DarkGray
            }),
        )
        .title(Span::styled(title, Style::default().fg(Color::Gray)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(protocol) = app.protocol.as_mut() {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Fit(None)),
            inner,
            protocol,
        );
        return;
    }

    if let Job::Running { started, phase, .. } = &app.job {
        let lines = marquee_lines(inner.width, app.tick, phase, started.elapsed());
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        return;
    }

    let placeholder = vec![
        Line::from(""),
        Line::from(Span::styled(
            "type a prompt below, enter to generate",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "drag & drop or paste an image path anywhere to attach a reference",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            format!("images land in {}", app.out_dir.display()),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(placeholder).alignment(Alignment::Center),
        inner,
    );
}

fn marquee_lines<'a>(width: u16, tick: u64, phase: &Phase, elapsed: Duration) -> Vec<Line<'a>> {
    let spinner = SPINNER[(tick as usize / 2) % SPINNER.len()];
    let phase_text = match phase {
        Phase::Queued => "queued",
        Phase::Generating => "generating",
        Phase::Finishing => "finishing",
    };
    let head = Line::from(vec![
        Span::styled(
            format!("{spinner} {phase_text} "),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:.1}s", elapsed.as_secs_f64()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let bar_width = usize::from(width).saturating_sub(6).clamp(10, 56);
    let pos = (tick as usize / 2) % (bar_width + 12);
    let mut spans = Vec::with_capacity(bar_width);
    for index in 0..bar_width {
        let distance = (index + 6).abs_diff(pos + 6);
        let (glyph, color) = if distance < 2 {
            ("█", Color::LightCyan)
        } else if distance < 4 {
            ("▓", Color::Cyan)
        } else if distance < 7 {
            ("▒", Color::Blue)
        } else {
            ("░", Color::DarkGray)
        };
        spans.push(Span::styled(glyph, Style::default().fg(color)));
    }
    vec![
        Line::from(""),
        head,
        Line::from(spans),
        Line::from(""),
        Line::from(Span::styled(
            "esc cancels · the image will pop in here",
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

fn draw_input(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let running = matches!(app.job, Job::Running { .. });
    let border = if running {
        ACCENTS[((app.tick / 3) % ACCENTS.len() as u64) as usize]
    } else {
        Color::Cyan
    };
    let mut title = String::from(" prompt ");
    if !app.refs.is_empty() {
        title.push_str(&format!("· {} ref ", app.refs.len()));
    }
    if let Some(last) = &app.last_image_path
        && let Some(name) = last.file_name().and_then(|name| name.to_str())
    {
        title.push_str(&format!("· editing {name} "));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(title, Style::default().fg(Color::Gray)))
        .padding(Padding::horizontal(1));
    let paragraph = Paragraph::new(app.display_input())
        .block(block)
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::White));
    frame.render_widget(paragraph, area);
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let (left, style) = match &app.job {
        Job::Running { started, phase, .. } => {
            let spinner = SPINNER[(app.tick as usize / 2) % SPINNER.len()];
            (
                format!(
                    "{spinner} {} {:.0}s",
                    phase.as_str(),
                    started.elapsed().as_secs_f32()
                ),
                Style::default().fg(Color::Yellow),
            )
        }
        Job::Idle => (
            app.status.clone(),
            Style::default().fg(match app.level {
                Level::Info => Color::Gray,
                Level::Ok => Color::Green,
                Level::Err => Color::Red,
            }),
        ),
    };
    let hints_text = "enter generate · shift+enter newline · tab quality · esc cancel/clear · ctrl-c quit ";
    let max_left = usize::from(area.width).saturating_sub(hints_text.chars().count() + 1);
    frame.render_widget(
        Paragraph::new(Span::styled(truncate_chars(&left, max_left), style)),
        area,
    );
    let hints = Line::from(Span::styled(hints_text, Style::default().fg(Color::DarkGray)));
    frame.render_widget(Paragraph::new(hints).alignment(Alignment::Right), area);
}

pub fn run_tui(args: TuiArgs) -> Result<i32> {
    let mut app = App::new(args)?;
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = app.run(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result?;
    Ok(0)
}

fn paste_reference(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed.trim_matches(|c| c == '\'' || c == '"');
    let unescaped = unquoted.replace("\\ ", " ");
    let path = PathBuf::from(files::expand_tilde(&unescaped));
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let is_image = [".png", ".jpg", ".jpeg", ".webp", ".gif"]
        .iter()
        .any(|extension| name.ends_with(extension));
    (is_image && path.is_file()).then_some(path)
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image")
        .to_string()
}

fn insert_text(input: &mut String, cursor: &mut usize, text: &str) {
    let mut chars: Vec<char> = input.chars().collect();
    let at = (*cursor).min(chars.len());
    for (offset, ch) in text.chars().enumerate() {
        chars.insert(at + offset, ch);
    }
    *input = chars.into_iter().collect();
    *cursor = at + text.chars().count();
}

fn backspace(input: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let mut chars: Vec<char> = input.chars().collect();
    let at = (*cursor).min(chars.len());
    chars.remove(at - 1);
    *input = chars.into_iter().collect();
    *cursor = at - 1;
}

fn delete_forward(input: &mut String, cursor: &mut usize) {
    let mut chars: Vec<char> = input.chars().collect();
    let at = (*cursor).min(chars.len());
    if at < chars.len() {
        chars.remove(at);
        *input = chars.into_iter().collect();
    }
}

fn word_delete(input: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let chars: Vec<char> = input.chars().collect();
    let mut at = (*cursor).min(chars.len());
    while at > 0 && chars[at - 1].is_whitespace() {
        at -= 1;
    }
    while at > 0 && !chars[at - 1].is_whitespace() {
        at -= 1;
    }
    let remaining: String = chars[..at]
        .iter()
        .chain(chars[(*cursor).min(chars.len())..].iter())
        .collect();
    *input = remaining;
    *cursor = at;
}

fn wrapped_lines(text: &str, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.split('\n')
        .map(|line| line.chars().count().div_ceil(width).max(1))
        .sum()
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_detects_image_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("ref image.png");
        std::fs::write(&image, [0x89, b'P', b'N', b'G']).unwrap();
        assert_eq!(
            paste_reference(image.to_str().unwrap()),
            Some(image.clone())
        );
        let escaped = image.to_str().unwrap().replace(' ', "\\ ");
        assert_eq!(paste_reference(&escaped), Some(image.clone()));
        assert_eq!(
            paste_reference(&format!("\"{}\"", image.display())),
            Some(image)
        );

        let text_file = tmp.path().join("notes.txt");
        std::fs::write(&text_file, b"hello").unwrap();
        assert_eq!(paste_reference(text_file.to_str().unwrap()), None);
        assert_eq!(paste_reference("just some text"), None);
    }

    #[test]
    fn editing_helpers_track_cursor() {
        let mut input = String::new();
        let mut cursor = 0;
        insert_text(&mut input, &mut cursor, "hello");
        insert_text(&mut input, &mut cursor, " world");
        assert_eq!(input, "hello world");
        assert_eq!(cursor, 11);
        backspace(&mut input, &mut cursor);
        assert_eq!(input, "hello worl");
        assert_eq!(cursor, 10);
        cursor = 5;
        delete_forward(&mut input, &mut cursor);
        assert_eq!(input, "helloworl");
        assert_eq!(cursor, 5);
        insert_text(&mut input, &mut cursor, " great ");
        assert_eq!(input, "hello great worl");
        word_delete(&mut input, &mut cursor);
        assert_eq!(input, "hello worl");
        assert_eq!(cursor, 6);
    }

    #[test]
    fn wrapped_line_count_is_sane() {
        assert_eq!(wrapped_lines("", 10), 1);
        assert_eq!(wrapped_lines("abc", 10), 1);
        assert_eq!(wrapped_lines(&"x".repeat(21), 10), 3);
        assert_eq!(wrapped_lines("one\ntwo", 10), 2);
    }
}
