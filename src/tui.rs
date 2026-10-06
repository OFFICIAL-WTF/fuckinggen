use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use std::io::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::api::{self, Phase, Spec};
use crate::args::TuiArgs;
use crate::config::{self, Config};
use crate::state;
use crate::{auth, files, http, images};

const TICK: Duration = Duration::from_millis(40);
const MAX_INPUT_ROWS: u16 = 12;
const MAX_GALLERY: usize = 16;
const AUTH_REFRESH_TICKS: u64 = 40;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const QUALITIES: [&str; 4] = ["low", "medium", "high", "auto"];
const THEMES: [(&str, Color); 8] = [
    ("blue", Color::Blue),
    ("lightblue", Color::LightBlue),
    ("cyan", Color::Cyan),
    ("magenta", Color::Magenta),
    ("green", Color::Green),
    ("yellow", Color::Yellow),
    ("white", Color::White),
    ("system", Color::Reset),
];

struct CommandSpec {
    name: &'static str,
    args: &'static str,
    description: &'static str,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/open",
        args: "",
        description: "open the selected image in the system viewer",
    },
    CommandSpec {
        name: "/view",
        args: "",
        description: "reveal the selected image in the file manager",
    },
    CommandSpec {
        name: "/root",
        args: "",
        description: "move session images to ~/Downloads and save there",
    },
    CommandSpec {
        name: "/save",
        args: "",
        description: "keep the selected image (moves it out of the session cache)",
    },
    CommandSpec {
        name: "/save-all",
        args: "",
        description: "keep every image from this session",
    },
    CommandSpec {
        name: "/ctx",
        args: "",
        description: "toggle handing the selected image to the next prompt",
    },
    CommandSpec {
        name: "/remove",
        args: "",
        description: "delete the selected image",
    },
    CommandSpec {
        name: "/remove-all",
        args: "",
        description: "delete every image from this session",
    },
    CommandSpec {
        name: "/quality",
        args: "low|medium|high|auto",
        description: "set generation quality",
    },
    CommandSpec {
        name: "/dir",
        args: "<path>",
        description: "save new images into another folder",
    },
    CommandSpec {
        name: "/settings",
        args: "",
        description: "theme, login, defaults",
    },
    CommandSpec {
        name: "/help",
        args: "",
        description: "keys and commands",
    },
    CommandSpec {
        name: "/clear",
        args: "",
        description: "clear the prompt",
    },
    CommandSpec {
        name: "/quit",
        args: "",
        description: "leave fuckinggen",
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Input,
    Button,
}

impl Focus {
    fn toggled(self) -> Self {
        match self {
            Focus::Input => Focus::Button,
            Focus::Button => Focus::Input,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Input,
    Button,
    Quality,
    Finish,
    Palette(usize),
    Reference(usize),
    Snake,
    SettingsRow(usize),
}

struct Reference {
    path: PathBuf,
    image: Option<image::DynamicImage>,
    protocol: Option<StatefulProtocol>,
}

struct GalleryItem {
    path: PathBuf,
    prompt: String,
    bytes: usize,
    /// Kept images already live in the save directory; the rest are staged in
    /// the session cache until the user decides.
    saved: bool,
    image: Option<image::DynamicImage>,
    protocol: Option<StatefulProtocol>,
}

/// One staged image in the "what do we keep" screen shown on the way out.
struct FinishRow {
    path: PathBuf,
    prompt: String,
    bytes: usize,
    keep: bool,
}

/// The exit checklist: every unsaved generation of this session, all ticked.
struct Finish {
    rows: Vec<FinishRow>,
    selection: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FinishButton {
    KeepTicked,
    Nothing,
}

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

enum RefMsg {
    Decoded {
        path: PathBuf,
        image: Option<image::DynamicImage>,
    },
}

struct Done {
    path: PathBuf,
    prompt: String,
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

#[derive(Default)]
struct Settings {
    open: bool,
    selection: usize,
}

const SETTINGS_ROWS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnakePoint {
    x: u16,
    y: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnakeDirection {
    Up,
    Down,
    Left,
    Right,
}

impl SnakeDirection {
    fn opposite(self) -> Self {
        match self {
            Self::Up => Self::Down,
            Self::Down => Self::Up,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

const SNAKE_STEP: Duration = Duration::from_millis(130);
/// The board is drawn as a square block in the middle of the output panel.
const MIN_SNAKE_SIDE: u16 = 8;
const MAX_SNAKE_SIDE: u16 = 28;

/// Biggest square board (in game cells) that fits in `area`. Every game cell
/// takes two terminal columns, so a square cell grid looks square on screen.
/// `None` when there is no room for a game worth playing.
fn snake_board_cells(area: Rect) -> Option<u16> {
    let columns = area.width.saturating_sub(2) / 2;
    let rows = area.height.saturating_sub(2);
    let side = columns.min(rows).min(MAX_SNAKE_SIDE);
    (side >= MIN_SNAKE_SIDE).then_some(side)
}

/// Circles shrink towards the tail: big circle, bullet, bullet operator, middle
/// dot, period — six grades (two circles) so the transition reads smooth rather
/// than stepped.
const SNAKE_TAPER: [&str; 6] = ["●", "●", "•", "∙", "·", "."];

/// Circle size by distance from the head, interpolated across the whole body.
fn segment_glyph(index: usize, len: usize) -> &'static str {
    if index == 0 || len <= 3 {
        return SNAKE_TAPER[0];
    }
    let grades = SNAKE_TAPER.len() - 1;
    let span = len - 1;
    // Rounded division: 0 at the head, `grades` at the tail.
    let grade = (index * grades + span / 2) / span;
    SNAKE_TAPER[grade.min(grades)]
}

struct SnakeGame {
    width: u16,
    height: u16,
    body: Vec<SnakePoint>,
    food: SnakePoint,
    direction: SnakeDirection,
    queued_direction: Option<SnakeDirection>,
    food_seed: u64,
    last_step: Instant,
    focused: bool,
    score: u32,
}

impl SnakeGame {
    fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            body: Vec::new(),
            food: SnakePoint { x: 0, y: 0 },
            direction: SnakeDirection::Right,
            queued_direction: None,
            food_seed: 1,
            last_step: Instant::now(),
            focused: false,
            score: 0,
        }
    }

    fn resize(&mut self, width: u16, height: u16) {
        let width = width.max(4);
        let height = height.max(3);
        if self.width == width && self.height == height && !self.body.is_empty() {
            return;
        }
        self.width = width;
        self.height = height;
        self.reset();
        self.place_food();
    }

    fn reset(&mut self) {
        let center = SnakePoint {
            x: self.width / 2,
            y: self.height / 2,
        };
        // Longer snake on a bigger board, so a big square does not start with a
        // stubby worm and the tail taper has room to show.
        let length = (usize::from(self.width) / 4).clamp(4, 10);
        self.body = (0..length)
            .map(|offset| SnakePoint {
                x: (i32::from(center.x) - offset as i32).rem_euclid(i32::from(self.width)) as u16,
                y: center.y,
            })
            .collect();
        self.direction = SnakeDirection::Right;
        self.queued_direction = None;
        self.score = 0;
    }

    fn activate(&mut self) {
        self.focused = true;
        self.last_step = Instant::now();
    }

    fn deactivate(&mut self) {
        self.focused = false;
    }

    fn set_direction(&mut self, direction: SnakeDirection) {
        if self.body.len() > 1 && direction == self.direction.opposite() {
            return;
        }
        // Turning should feel instant: if this really is a new direction, let
        // the next tick step instead of waiting out the rest of the interval.
        let turning = direction != self.direction;
        self.queued_direction = Some(direction);
        if turning && self.focused {
            self.last_step = Instant::now() - SNAKE_STEP;
        }
    }

    fn tick(&mut self) {
        if !self.focused || self.last_step.elapsed() < SNAKE_STEP {
            return;
        }
        self.last_step = Instant::now();
        self.step();
    }

    fn step(&mut self) {
        if !self.focused || self.width == 0 || self.height == 0 {
            return;
        }
        if let Some(direction) = self.queued_direction.take()
            && (self.body.len() <= 1 || direction != self.direction.opposite())
        {
            self.direction = direction;
        }
        let head = self.body[0];
        let (dx, dy) = match self.direction {
            SnakeDirection::Up => (0, -1),
            SnakeDirection::Down => (0, 1),
            SnakeDirection::Left => (-1, 0),
            SnakeDirection::Right => (1, 0),
        };
        let next = SnakePoint {
            x: (i32::from(head.x) + dx).rem_euclid(i32::from(self.width)) as u16,
            y: (i32::from(head.y) + dy).rem_euclid(i32::from(self.height)) as u16,
        };
        let eats = next == self.food;
        let collision_end = if eats {
            self.body.len()
        } else {
            self.body.len().saturating_sub(1)
        };
        if self.body[..collision_end].contains(&next) {
            self.reset();
            self.place_food();
            return;
        }
        self.body.insert(0, next);
        if eats {
            self.score = self.score.saturating_add(1);
            self.place_food();
        } else {
            self.body.pop();
        }
    }

    fn place_food(&mut self) {
        let cells = u64::from(self.width) * u64::from(self.height);
        for offset in 0..cells {
            let index = (self.food_seed + offset) % cells;
            let point = SnakePoint {
                x: (index % u64::from(self.width)) as u16,
                y: (index / u64::from(self.width)) as u16,
            };
            if !self.body.contains(&point) {
                self.food = point;
                self.food_seed = index.wrapping_add(17);
                return;
            }
        }
        self.food = self.body[0];
    }
}

struct App {
    picker: Picker,
    input: String,
    cursor: usize,
    refs: Vec<Reference>,
    ref_tx: mpsc::Sender<RefMsg>,
    ref_rx: mpsc::Receiver<RefMsg>,
    gallery: Vec<GalleryItem>,
    selected: Option<usize>,
    job: Job,
    tick: u64,
    status: String,
    level: Level,
    out_dir: PathBuf,
    quality: String,
    accent: Color,
    config: Config,
    focus: Focus,
    hover: Option<Hit>,
    snake: SnakeGame,
    hit_input: Rect,
    hit_button: Rect,
    hit_quality: Rect,
    hit_finish_button: Rect,
    hit_palette: Vec<(Rect, usize)>,
    hit_refs: Vec<(Rect, usize)>,
    hit_settings: Vec<(Rect, usize)>,
    snake_area: Rect,
    palette: Vec<usize>,
    palette_selection: usize,
    settings: Settings,
    /// Scratch directory for generations the user has not decided about yet.
    session_dir: PathBuf,
    finish: Option<Finish>,
    forced_quit: bool,
    /// Follow-up generations hand the selected image back to the model, since
    /// the backend request is stateless. `/ctx` turns it off.
    carry_context: bool,
    hit_finish_rows: Vec<(Rect, usize)>,
    hit_finish_buttons: Vec<(Rect, FinishButton)>,
    help_open: bool,
    auth_ok: bool,
    auth_note: String,
    quit: bool,
    pending_login: bool,
}

impl App {
    fn new(args: TuiArgs) -> Result<Self> {
        let picker = terminal_picker();
        let config = config::load();
        let accent = accent_from_config(config.accent.as_deref());
        let quality = config
            .quality
            .clone()
            .filter(|value| QUALITIES.contains(&value.as_str()))
            .unwrap_or_else(|| "auto".to_string());
        let out_dir = match args.out_dir.as_deref() {
            Some(dir) => PathBuf::from(files::expand_tilde(dir)),
            None => match config.out_dir.as_deref() {
                Some(dir) => PathBuf::from(files::expand_tilde(dir)),
                None => std::env::current_dir().context("reading current directory")?,
            },
        };
        let session_dir = files::cache_dir().join(format!(
            "session-{}-{}",
            std::process::id(),
            files::now_unix()
        ));
        std::fs::create_dir_all(&session_dir)
            .with_context(|| format!("creating {}", session_dir.display()))?;
        let input = args.initial_prompt.unwrap_or_default();
        let cursor = input.chars().count();
        let (ref_tx, ref_rx) = mpsc::channel();
        let mut app = App {
            picker,
            input,
            cursor,
            refs: Vec::new(),
            ref_tx,
            ref_rx,
            gallery: Vec::new(),
            selected: None,
            job: Job::Idle,
            tick: 0,
            status: format!("saving to {}", out_dir.display()),
            level: Level::Info,
            out_dir,
            quality,
            accent,
            config,
            focus: Focus::Input,
            snake: SnakeGame::new(),
            hover: None,
            hit_input: Rect::default(),
            hit_button: Rect::default(),
            hit_quality: Rect::default(),
            hit_finish_button: Rect::default(),
            hit_palette: Vec::new(),
            hit_refs: Vec::new(),
            hit_settings: Vec::new(),
            snake_area: Rect::default(),
            palette: Vec::new(),
            palette_selection: 0,
            settings: Settings::default(),
            session_dir: session_dir.clone(),
            finish: None,
            forced_quit: false,
            carry_context: true,
            hit_finish_rows: Vec::new(),
            hit_finish_buttons: Vec::new(),
            help_open: false,
            auth_ok: false,
            auth_note: String::new(),
            quit: false,
            pending_login: false,
        };
        app.refresh_auth();
        app.refresh_palette();
        Ok(app)
    }

    fn accent(&self) -> Color {
        self.accent
    }

    fn refresh_auth(&mut self) {
        match auth::load(None) {
            Ok(cred) if cred.valid() => {
                self.auth_ok = true;
                self.auth_note = cred.masked_account();
            }
            Ok(cred) => {
                self.auth_ok = false;
                self.auth_note = match cred.expires_at {
                    Some(at) => format!("expired {}", files::rfc3339(at)),
                    None => "expired".to_string(),
                };
            }
            Err(_) => {
                self.auth_ok = false;
                self.auth_note = "run codex login".to_string();
            }
        }
    }

    fn save_config(&mut self) {
        self.config.accent = Some(theme_name(self.accent).to_string());
        self.config.quality = Some(self.quality.clone());
        self.config.out_dir = Some(self.out_dir.display().to_string());
        let _ = config::save(&self.config);
    }

    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        while !self.quit {
            if self.pending_login {
                self.pending_login = false;
                ratatui::restore();
                let outcome = Command::new("codex").arg("login").status();
                *terminal = ratatui::init();
                let _ = execute!(std::io::stdout(), EnableBracketedPaste);
                let _ = std::io::stdout().write_all(b"\x1b[?1000h\x1b[?1006h");
                let _ = std::io::stdout().flush();
                self.refresh_auth();
                match outcome {
                    Ok(status) if status.success() => {
                        self.set_status(Level::Ok, "codex login finished")
                    }
                    _ => {
                        self.set_status(Level::Err, "codex login failed — try it in a normal shell")
                    }
                }
            }
            if matches!(self.job, Job::Running { .. }) {
                self.snake.tick();
            } else {
                self.snake.deactivate();
            }
            terminal.draw(|frame| draw(frame, self))?;
            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(key) => self.on_key(key),
                    Event::Paste(text) => self.on_paste(&text),
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    _ => {}
                }
            }
            self.tick += 1;
            if self.tick.is_multiple_of(AUTH_REFRESH_TICKS) {
                self.refresh_auth();
            }
            self.drain_worker();
            self.drain_ref_decodes();
        }
        Ok(())
    }

    fn set_status(&mut self, level: Level, text: impl Into<String>) {
        self.level = level;
        self.status = text.into();
    }

    fn refresh_palette(&mut self) {
        self.palette = palette_matches(&self.input);
        if self.palette_selection >= self.palette.len() {
            self.palette_selection = 0;
        }
    }

    fn current_item(&self) -> Option<&GalleryItem> {
        self.selected.and_then(|index| self.gallery.get(index))
    }

    fn hit_test(&self, column: u16, row: u16) -> Option<Hit> {
        let position = Position::new(column, row);
        for (rect, index) in &self.hit_settings {
            if rect.contains(position) {
                return Some(Hit::SettingsRow(*index));
            }
        }
        for (rect, index) in &self.hit_palette {
            if rect.contains(position) {
                return Some(Hit::Palette(*index));
            }
        }
        for (rect, index) in &self.hit_refs {
            if rect.contains(position) {
                return Some(Hit::Reference(*index));
            }
        }
        if self.snake_area.contains(position) {
            return Some(Hit::Snake);
        }
        if self.hit_button.contains(position) {
            return Some(Hit::Button);
        }
        if self.hit_quality.contains(position) {
            return Some(Hit::Quality);
        }
        if self.hit_finish_button.contains(position) {
            return Some(Hit::Finish);
        }
        if self.hit_input.contains(position) {
            return Some(Hit::Input);
        }
        hit_test_static(
            self.hit_button,
            self.hit_quality,
            self.hit_input,
            column,
            row,
        )
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.finish.is_some() {
            self.on_finish_mouse(mouse);
            return;
        }
        let hit = self.hit_test(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Moved => self.hover = hit,
            MouseEventKind::Down(MouseButton::Left) => {
                self.hover = hit;
                match hit {
                    Some(Hit::Button) => {
                        self.focus = Focus::Button;
                        self.submit();
                    }
                    Some(Hit::Quality) => {
                        self.focus = Focus::Input;
                        self.cycle_quality();
                    }
                    Some(Hit::Finish) => self.request_quit(),
                    Some(Hit::Input) => self.focus = Focus::Input,
                    Some(Hit::Palette(index)) => {
                        self.focus = Focus::Input;
                        self.palette_selection = index;
                        self.complete_palette();
                    }
                    Some(Hit::Reference(index)) => self.remove_reference(index),
                    Some(Hit::Snake) => {
                        self.snake.activate();
                        self.set_status(Level::Info, "snake · arrows move · edges wrap");
                    }
                    Some(Hit::SettingsRow(index)) => {
                        self.settings.selection = index;
                        self.activate_setting();
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn on_paste(&mut self, text: &str) {
        let paths = drop_paths(text);
        if !paths.is_empty() {
            for path in paths {
                self.attach_reference(path);
            }
            return;
        }
        self.focus = Focus::Input;
        insert_text(&mut self.input, &mut self.cursor, text);
        self.refresh_palette();
        self.promote_input_paths();
    }

    /// Attach a reference immediately; decoding happens on a worker thread so a
    /// slow disk (iCloud, network shares) or a 30-megapixel drag can never
    /// freeze the UI.
    fn attach_reference(&mut self, path: PathBuf) {
        if self.refs.iter().any(|item| item.path == path) {
            self.set_status(
                Level::Info,
                format!("{} is already attached", display_name(&path)),
            );
            return;
        }
        self.refs.push(Reference {
            path: path.clone(),
            image: None,
            protocol: None,
        });
        self.set_status(Level::Ok, format!("attached {}", display_name(&path)));
        let tx = self.ref_tx.clone();
        std::thread::spawn(move || {
            let source = path.clone();
            let decoded = catch_unwind(AssertUnwindSafe(move || {
                let bytes = std::fs::read(&source).ok()?;
                let image = image::load_from_memory(&bytes).ok()?;
                // Thumbnails never need the full resolution; shrinking here keeps
                // rendering cheap and memory bounded.
                Some(image.thumbnail(1024, 1024))
            }))
            .ok()
            .flatten();
            let _ = tx.send(RefMsg::Decoded {
                path: path.clone(),
                image: decoded,
            });
        });
    }

    fn drain_ref_decodes(&mut self) {
        while let Ok(RefMsg::Decoded { path, image }) = self.ref_rx.try_recv() {
            if let Some(item) = self.refs.iter_mut().find(|item| item.path == path)
                && item.image.is_none()
            {
                item.image = image;
            }
        }
    }

    /// A path typed or dropped straight into the prompt becomes a reference
    /// instead of a prompt that happens to look like a path.
    fn promote_input_paths(&mut self) -> bool {
        let Some(paths) = input_paths(&self.input) else {
            return false;
        };
        self.input.clear();
        self.cursor = 0;
        self.refresh_palette();
        for path in paths {
            self.attach_reference(path);
        }
        true
    }

    fn remove_reference(&mut self, index: usize) {
        if index < self.refs.len() {
            let item = self.refs.remove(index);
            self.set_status(Level::Info, format!("removed {}", display_name(&item.path)));
        }
    }

    fn cycle_quality(&mut self) {
        let index = QUALITIES
            .iter()
            .position(|quality| *quality == self.quality.as_str())
            .unwrap_or(QUALITIES.len() - 1);
        self.quality = QUALITIES[(index + 1) % QUALITIES.len()].to_string();
        let quality = self.quality.clone();
        self.save_config();
        self.set_status(Level::Info, format!("quality {quality}"));
    }

    fn cycle_theme(&mut self, forward: bool) {
        let index = THEMES
            .iter()
            .position(|(_, color)| *color == self.accent)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % THEMES.len()
        } else {
            (index + THEMES.len() - 1) % THEMES.len()
        };
        self.accent = THEMES[next].1;
        self.save_config();
        let name = THEMES[next].0;
        self.set_status(Level::Info, format!("theme {name}"));
    }

    fn complete_palette(&mut self) {
        if let Some(index) = self.palette.get(self.palette_selection).copied() {
            let spec = &COMMANDS[index];
            let args: String = self
                .input
                .split_once(' ')
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_default();
            self.input = if spec.args.is_empty() || !args.is_empty() {
                format!("{} {}", spec.name, args)
            } else {
                format!("{} ", spec.name)
            };
            self.cursor = self.input.chars().count();
            self.refresh_palette();
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        if self.help_open {
            self.help_open = false;
            return;
        }
        if self.settings.open {
            self.on_settings_key(key);
            return;
        }
        if self.finish.is_some() {
            // Inside the checklist, a second ctrl-c means "just get out".
            if matches!(
                (key.code, key.modifiers),
                (KeyCode::Char('c'), KeyModifiers::CONTROL)
            ) {
                self.forced_quit = true;
                self.quit = true;
                return;
            }
            self.on_finish_key(key);
            return;
        }
        if self.snake.focused && self.snake_area.width > 0 && key.modifiers == KeyModifiers::NONE {
            let direction = match key.code {
                KeyCode::Up => Some(SnakeDirection::Up),
                KeyCode::Down => Some(SnakeDirection::Down),
                KeyCode::Left => Some(SnakeDirection::Left),
                KeyCode::Right => Some(SnakeDirection::Right),
                _ => None,
            };
            if let Some(direction) = direction {
                self.snake.set_direction(direction);
                return;
            }
        }

        let palette_open = !self.palette.is_empty();
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('q'), KeyModifiers::CONTROL) => self.request_quit(),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.focus = Focus::Input;
                self.input.clear();
                self.cursor = 0;
                self.refresh_palette();
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                self.focus = Focus::Input;
                word_delete(&mut self.input, &mut self.cursor);
                self.refresh_palette();
            }
            (KeyCode::Char('t'), KeyModifiers::CONTROL) => self.cycle_quality(),
            (KeyCode::Char('p'), KeyModifiers::CONTROL) => self.select_gallery(-1),
            (KeyCode::Char('n'), KeyModifiers::CONTROL) => self.select_gallery(1),
            (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                self.focus = Focus::Input;
                insert_text(&mut self.input, &mut self.cursor, "\n");
                self.refresh_palette();
            }
            (KeyCode::Enter, modifiers) if is_newline_modifier(modifiers) => {
                self.focus = Focus::Input;
                insert_text(&mut self.input, &mut self.cursor, "\n");
                self.refresh_palette();
            }
            (KeyCode::Esc, _) => {
                if self.snake.focused {
                    self.snake.deactivate();
                    self.focus = Focus::Input;
                    self.set_status(Level::Info, "snake paused");
                } else if let Job::Running { cancel, .. } = &self.job {
                    cancel.store(true, Ordering::Relaxed);
                    self.set_status(Level::Info, "cancelling…");
                } else {
                    self.focus = Focus::Input;
                    self.input.clear();
                    self.cursor = 0;
                    self.refresh_palette();
                }
            }
            (KeyCode::Up, _) if palette_open => {
                self.palette_selection =
                    (self.palette_selection + self.palette.len() - 1) % self.palette.len();
            }
            (KeyCode::Down, _) if palette_open => {
                self.palette_selection = (self.palette_selection + 1) % self.palette.len();
            }
            (KeyCode::Up, _) => self.select_gallery(-1),
            (KeyCode::Down, _) => self.select_gallery(1),
            (KeyCode::Tab, _) if palette_open => self.complete_palette(),
            (KeyCode::Tab, _) => {
                self.focus = self.focus.toggled();
            }
            (KeyCode::Enter, _) => {
                if self.promote_input_paths() {
                    // the prompt was a path; it attached as a reference instead
                } else if self.is_command_line() {
                    let token = self
                        .input
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    let exact = COMMANDS.iter().any(|spec| spec.name == token);
                    if exact {
                        let line = self.input.clone();
                        self.input.clear();
                        self.cursor = 0;
                        self.refresh_palette();
                        self.execute_command(&line);
                    } else if palette_open {
                        self.complete_palette();
                        self.set_status(Level::Info, "pick a command · enter runs it");
                    } else {
                        self.set_status(
                            Level::Err,
                            format!("unknown command {token} — /help lists them"),
                        );
                        self.input.clear();
                        self.cursor = 0;
                        self.refresh_palette();
                    }
                } else if self.input.trim().is_empty() && self.selected.is_some() {
                    // Enter on an empty prompt keeps the image you are looking at.
                    self.save_selected();
                } else {
                    self.submit();
                }
            }
            (KeyCode::Char(' '), KeyModifiers::NONE)
                if self.focus == Focus::Button && self.input.is_empty() =>
            {
                self.submit()
            }
            (KeyCode::Backspace, _) => {
                self.focus = Focus::Input;
                backspace(&mut self.input, &mut self.cursor);
                self.refresh_palette();
            }
            (KeyCode::Delete, _) => {
                self.focus = Focus::Input;
                delete_forward(&mut self.input, &mut self.cursor);
                self.refresh_palette();
            }
            (KeyCode::Left, _) => self.cursor = self.cursor.saturating_sub(1),
            (KeyCode::Right, _) => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            (KeyCode::Home, _) | (KeyCode::PageUp, _) => self.cursor = 0,
            (KeyCode::End, _) | (KeyCode::PageDown, _) => self.cursor = self.input.chars().count(),
            (KeyCode::Char(c), modifiers)
                if !modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.focus = Focus::Input;
                insert_text(&mut self.input, &mut self.cursor, &c.to_string());
                self.refresh_palette();
            }
            _ => {}
        }
    }

    fn on_settings_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                self.settings.open = false;
            }
            (KeyCode::Up, _) => {
                self.settings.selection =
                    (self.settings.selection + SETTINGS_ROWS - 1) % SETTINGS_ROWS;
            }
            (KeyCode::Down, _) => {
                self.settings.selection = (self.settings.selection + 1) % SETTINGS_ROWS;
            }
            (KeyCode::Left, _) => match self.settings.selection {
                0 => self.cycle_theme(false),
                1 => self.cycle_quality_backwards(),
                _ => {}
            },
            (KeyCode::Right, _) => match self.settings.selection {
                0 => self.cycle_theme(true),
                1 => self.cycle_quality(),
                _ => {}
            },
            (KeyCode::Enter, _) => self.activate_setting(),
            _ => {}
        }
    }

    fn cycle_quality_backwards(&mut self) {
        let index = QUALITIES
            .iter()
            .position(|quality| *quality == self.quality.as_str())
            .unwrap_or(0);
        self.quality = QUALITIES[(index + QUALITIES.len() - 1) % QUALITIES.len()].to_string();
        self.save_config();
    }

    fn activate_setting(&mut self) {
        match self.settings.selection {
            0 => self.cycle_theme(true),
            1 => self.cycle_quality(),
            2 => {
                if let Some(home) = files::home_dir() {
                    self.out_dir = home.join("Downloads");
                    self.save_config();
                    let dir = self.out_dir.display().to_string();
                    self.set_status(Level::Info, format!("saving to {dir}"));
                }
            }
            3 => {
                self.settings.open = false;
                self.pending_login = true;
            }
            4 => {
                self.refresh_auth();
                let note = self.auth_note.clone();
                if self.auth_ok {
                    self.set_status(Level::Ok, format!("codex connected · {note}"));
                } else {
                    self.set_status(Level::Err, format!("not connected · {note}"));
                }
            }
            _ => self.settings.open = false,
        }
    }

    fn is_command_line(&self) -> bool {
        command_token(&self.input).is_some()
    }

    fn select_gallery(&mut self, delta: i32) {
        let next = next_selection(self.gallery.len(), self.selected, delta);
        if next != self.selected {
            self.selected = next;
            if let Some(item) = next.and_then(|index| self.gallery.get(index)) {
                let name = display_name(&item.path);
                self.set_status(Level::Info, format!("showing {name}"));
            }
        }
    }

    fn execute_command(&mut self, line: &str) {
        let trimmed = line.trim();
        let mut parts = trimmed.split_whitespace();
        let name = parts.next().unwrap_or_default();
        let args: Vec<&str> = parts.collect();
        match name {
            "/open" => self.reveal(false),
            "/view" => self.reveal(true),
            "/root" => self.move_to_downloads(),
            "/save" => self.save_selected(),
            "/save-all" => self.save_all_staged(),
            "/ctx" => {
                self.carry_context = !self.carry_context;
                let state = if self.carry_context { "on" } else { "off" };
                self.set_status(Level::Info, format!("follow-up context {state}"));
            }
            "/remove" => self.remove_selected(),
            "/remove-all" => self.remove_all(),
            "/quality" => match args.first() {
                Some(value) if QUALITIES.contains(value) => {
                    self.quality = (*value).to_string();
                    self.save_config();
                    let quality = self.quality.clone();
                    self.set_status(Level::Info, format!("quality {quality}"));
                }
                Some(value) => {
                    let value = value.to_string();
                    self.set_status(
                        Level::Err,
                        format!("quality must be low|medium|high|auto, got {value}"),
                    );
                }
                None => self.cycle_quality(),
            },
            "/dir" => match args.first() {
                Some(value) => {
                    let dir = PathBuf::from(files::expand_tilde(value));
                    match std::fs::create_dir_all(&dir) {
                        Ok(()) => {
                            self.out_dir = dir;
                            self.save_config();
                            let dir = self.out_dir.display().to_string();
                            self.set_status(Level::Ok, format!("saving to {dir}"));
                        }
                        Err(err) => {
                            let err = err.to_string();
                            self.set_status(Level::Err, format!("could not create {value}: {err}"));
                        }
                    }
                }
                None => self.set_status(Level::Err, "usage: /dir <path>".to_string()),
            },
            "/settings" => {
                self.settings.open = true;
                self.settings.selection = 0;
            }
            "/help" => self.help_open = true,
            "/clear" => {
                self.input.clear();
                self.cursor = 0;
                self.refresh_palette();
            }
            "/quit" => self.quit = true,
            other => self.set_status(Level::Err, format!("unknown command {other} — try /help")),
        }
    }

    fn reveal(&mut self, in_finder: bool) {
        let Some(item) = self.current_item() else {
            self.set_status(Level::Err, "nothing generated yet");
            return;
        };
        let path = item.path.clone();
        #[cfg(target_os = "macos")]
        let outcome = if in_finder {
            Command::new("open").arg("-R").arg(&path).spawn()
        } else {
            Command::new("open").arg(&path).spawn()
        };
        // Windows has no "reveal" verb, but explorer's /select, does the same
        // job; `start` (with the empty window-title argument) opens the file
        // with whatever is registered for the extension.
        #[cfg(target_os = "windows")]
        let outcome = if in_finder {
            Command::new("explorer")
                .arg(format!("/select,{}", path.display()))
                .spawn()
        } else {
            Command::new("cmd")
                .args(["/C", "start", ""])
                .arg(&path)
                .spawn()
        };
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let outcome = {
            let target = if in_finder {
                path.parent().unwrap_or(&path).to_path_buf()
            } else {
                path.clone()
            };
            // Most Linux desktop file managers implement the FileManager1
            // interface, which gives a real "reveal the file" behaviour;
            // xdg-open can only open the folder.
            if in_finder {
                let uri = format!("file://{}", path.display());
                Command::new("dbus-send")
                    .args(file_manager_args(&uri))
                    .spawn()
                    .or_else(|_| Command::new("xdg-open").arg(&target).spawn())
            } else {
                Command::new("xdg-open").arg(&target).spawn()
            }
        };
        match outcome {
            Ok(_) => {
                let action = if in_finder { "revealed" } else { "opened" };
                self.set_status(Level::Ok, format!("{action} {}", path.display()));
            }
            Err(err) => {
                let err = err.to_string();
                self.set_status(Level::Err, format!("open failed: {err}"));
            }
        }
    }

    /// Move one staged generation into the save directory for good.
    fn save_item(&mut self, index: usize) -> bool {
        let Some(item) = self.gallery.get(index) else {
            return false;
        };
        if item.saved {
            let name = display_name(&item.path);
            self.set_status(Level::Info, format!("{name} is already saved"));
            return false;
        }
        if !item.path.is_file() {
            self.set_status(Level::Err, "that file is gone".to_string());
            return false;
        }
        let prompt = item.prompt.clone();
        let staged = item.path.clone();
        let requested = files::resolve_out_path(None, Some(&self.out_dir), &prompt, &staged);
        let target = match files::prepare_path(&requested) {
            Ok(path) => path,
            Err(err) => {
                let err = err.to_string();
                self.set_status(Level::Err, format!("could not save: {err}"));
                return false;
            }
        };
        if let Err(err) = files::move_file(&staged, &target) {
            let err = err.to_string();
            self.set_status(Level::Err, format!("could not save: {err}"));
            return false;
        }
        let _ = state::record(&target, &prompt);
        if let Some(item) = self.gallery.get_mut(index) {
            item.path = target.clone();
            item.saved = true;
        }
        self.set_status(Level::Ok, format!("saved {}", target.display()));
        true
    }

    fn save_selected(&mut self) {
        let Some(index) = self.selected else {
            self.set_status(Level::Err, "nothing generated yet");
            return;
        };
        self.save_item(index);
    }

    fn save_all_staged(&mut self) {
        let unsaved: Vec<usize> = self
            .gallery
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.saved)
            .map(|(index, _)| index)
            .collect();
        if unsaved.is_empty() {
            self.set_status(Level::Info, "everything here is already saved");
            return;
        }
        let saved = unsaved
            .into_iter()
            .filter(|index| self.save_item(*index))
            .count();
        let dir = self.out_dir.display().to_string();
        self.set_status(Level::Ok, format!("saved {saved} image(s) to {dir}"));
    }

    fn unsaved_count(&self) -> usize {
        self.gallery.iter().filter(|item| !item.saved).count()
    }

    /// Quitting runs through the keep/discard checklist unless there is nothing
    /// staged, in which case there is nothing to ask about.
    fn request_quit(&mut self) {
        if self.finish.is_some() {
            return;
        }
        if let Job::Running { cancel, .. } = &self.job {
            cancel.store(true, Ordering::Relaxed);
            self.set_status(Level::Info, "cancelling…");
        }
        let rows: Vec<FinishRow> = self
            .gallery
            .iter()
            .filter(|item| !item.saved)
            .map(|item| FinishRow {
                path: item.path.clone(),
                prompt: item.prompt.clone(),
                bytes: item.bytes,
                keep: true,
            })
            .collect();
        if rows.is_empty() {
            self.quit = true;
            return;
        }
        self.finish = Some(Finish { rows, selection: 0 });
    }

    /// Apply the checklist: keep what is ticked, throw away what is not.
    fn finish_save(&mut self) {
        let Some(finish) = self.finish.take() else {
            return;
        };
        let outcome = apply_finish(&finish.rows, &self.out_dir);
        for saved in &outcome.saved {
            let _ = state::record(&saved.to, &saved.prompt);
            if let Some(item) = self.gallery.iter_mut().find(|item| item.path == saved.from) {
                item.path = saved.to.clone();
                item.saved = true;
            }
        }
        for path in &outcome.discarded {
            self.gallery.retain(|item| item.path != *path);
        }
        let kept = outcome.saved.len();
        let dropped = outcome.discarded.len();
        if kept > 0 {
            self.set_status(
                Level::Ok,
                format!("saved {kept} image(s) to {}", self.out_dir.display()),
            );
        } else if dropped > 0 {
            self.set_status(Level::Info, format!("discarded {dropped} image(s)"));
        }
        self.quit = true;
    }

    fn finish_discard(&mut self) {
        let Some(finish) = self.finish.take() else {
            return;
        };
        let count = finish.rows.len();
        for row in &finish.rows {
            let _ = std::fs::remove_file(&row.path);
        }
        self.gallery.retain(|item| item.saved);
        self.set_status(Level::Info, format!("discarded {count} image(s)"));
        self.quit = true;
    }

    fn on_finish_mouse(&mut self, mouse: MouseEvent) {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return;
        }
        let position = Position::new(mouse.column, mouse.row);
        for (rect, button) in &self.hit_finish_buttons {
            if rect.contains(position) {
                match button {
                    FinishButton::KeepTicked => self.finish_save(),
                    FinishButton::Nothing => self.finish_discard(),
                }
                return;
            }
        }
        for (rect, index) in &self.hit_finish_rows {
            if rect.contains(position) {
                if let Some(finish) = self.finish.as_mut() {
                    finish.selection = *index;
                    if let Some(row) = finish.rows.get_mut(*index) {
                        row.keep = !row.keep;
                    }
                }
                return;
            }
        }
    }

    fn on_finish_key(&mut self, key: KeyEvent) {
        let Some(finish) = self.finish.as_mut() else {
            return;
        };
        let len = finish.rows.len();
        match (key.code, key.modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                finish.selection = (finish.selection + len - 1) % len;
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                finish.selection = (finish.selection + 1) % len;
            }
            (KeyCode::Char(' '), KeyModifiers::NONE)
            | (KeyCode::Char(' '), KeyModifiers::SHIFT) => {
                if let Some(row) = finish.rows.get_mut(finish.selection) {
                    row.keep = !row.keep;
                }
            }
            (KeyCode::Enter, _) | (KeyCode::Char('a'), KeyModifiers::NONE) => self.finish_save(),
            (KeyCode::Char('n'), KeyModifiers::NONE) => self.finish_discard(),
            (KeyCode::Esc, _) => {
                self.finish = None;
                self.set_status(Level::Info, "nothing was changed");
            }
            _ => {}
        }
    }

    fn remove_selected(&mut self) {
        let Some(index) = self.selected else {
            self.set_status(Level::Err, "nothing generated yet");
            return;
        };
        let item = self.gallery.remove(index);
        let _ = std::fs::remove_file(&item.path);
        let _ = state::forget(&item.path);
        self.selected = if self.gallery.is_empty() {
            None
        } else {
            Some(index.min(self.gallery.len() - 1))
        };
        self.set_status(Level::Ok, format!("removed {}", item.path.display()));
    }

    fn remove_all(&mut self) {
        if self.gallery.is_empty() {
            self.set_status(Level::Err, "nothing generated yet");
            return;
        }
        let count = self.gallery.len();
        for item in self.gallery.drain(..) {
            let _ = std::fs::remove_file(&item.path);
            let _ = state::forget(&item.path);
        }
        self.selected = None;
        self.set_status(
            Level::Ok,
            format!("removed {count} images from this session"),
        );
    }

    fn move_to_downloads(&mut self) {
        let Some(home) = files::home_dir() else {
            self.set_status(Level::Err, "no home directory");
            return;
        };
        let target_dir = home.join("Downloads");
        if let Err(err) = std::fs::create_dir_all(&target_dir) {
            let err = err.to_string();
            self.set_status(Level::Err, format!("could not create Downloads: {err}"));
            return;
        }
        let mut moved = 0usize;
        for item in &mut self.gallery {
            if item.path.parent() == Some(target_dir.as_path()) {
                continue;
            }
            let file_name = item
                .path
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("image.png"));
            let mut target = target_dir.join(file_name);
            if target.exists() {
                target = files::pick_non_overwrite(&target, 999).unwrap_or(target);
            }
            let old = item.path.clone();
            let ok = std::fs::rename(&old, &target).is_ok()
                || (std::fs::copy(&old, &target).is_ok() && std::fs::remove_file(&old).is_ok());
            if ok {
                let _ = state::forget(&old);
                let _ = state::record(&target, &item.prompt);
                item.path = target;
                moved += 1;
            }
        }
        self.out_dir = target_dir;
        self.save_config();
        let dir = self.out_dir.display().to_string();
        self.set_status(Level::Ok, format!("moved {moved} images to {dir}"));
    }

    fn submit(&mut self) {
        if matches!(self.job, Job::Running { .. }) {
            return;
        }
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let mut ref_paths: Vec<PathBuf> = self.refs.iter().map(|item| item.path.clone()).collect();
        // Continuity: the backend keeps no conversation state (`store: false`),
        // so a follow-up only makes sense with the previous picture attached.
        if self.carry_context
            && let Some(item) = self.selected.and_then(|index| self.gallery.get(index))
            && !ref_paths.contains(&item.path)
        {
            ref_paths.push(item.path.clone());
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        // Generations are staged in the session cache; the user decides on the
        // way out (or with /save) what actually lands in `out_dir`.
        spawn_worker(
            prompt,
            ref_paths,
            self.session_dir.clone(),
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
        // The board shows up with the generation; nobody should have to click
        // anything before the arrow keys steer it.
        self.snake.activate();
        self.input.clear();
        self.cursor = 0;
        self.refresh_palette();
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
                    self.gallery.push(GalleryItem {
                        path: done.path.clone(),
                        prompt: done.prompt.clone(),
                        bytes: done.bytes,
                        saved: false,
                        image: Some(done.image.thumbnail(2048, 2048)),
                        protocol: None,
                    });
                    while self.gallery.len() > MAX_GALLERY {
                        // Dropping the oldest staged render also drops its file:
                        // the cache is scratch, not a save directory.
                        let dropped = self.gallery.remove(0);
                        if !dropped.saved {
                            let _ = std::fs::remove_file(&dropped.path);
                        }
                        if let Some(selected) = self.selected {
                            self.selected = Some(selected.saturating_sub(1));
                        }
                    }
                    self.selected = Some(self.gallery.len() - 1);
                    self.set_status(
                        Level::Ok,
                        format!(
                            "generated {} ({:.1}s, {}) · staged, enter keeps it",
                            display_name(&done.path),
                            done.elapsed.as_secs_f64(),
                            human_bytes(done.bytes)
                        ),
                    );
                    self.job = Job::Idle;
                    return;
                }
                Ok(WorkerMsg::Failed(err)) => {
                    self.set_status(Level::Err, truncate_chars(&err, 160));
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
                prompt,
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

/// Build the image-protocol picker. The kitty and iTerm2 protocols carry a real
/// alpha channel, so transparent PNGs keep their transparency there. Half-blocks
/// and sixels cannot, so for those we composite onto the terminal's own
/// background colour — a transparent picture then blends in instead of showing
/// up as a black or wrongly coloured block.
fn terminal_picker() -> Picker {
    let options = ratatui_image::picker::cap_parser::QueryStdioOptions {
        terminal_background_color_osc: true,
        ..Default::default()
    };
    let picker = Picker::from_query_stdio_with_options(options);
    let mut picker = match picker {
        Ok(picker) => picker,
        Err(_) => Picker::halfblocks(),
    };
    if !protocol_keeps_alpha(picker.protocol_type()) {
        let background = picker.capabilities().iter().find_map(|capability| {
            if let ratatui_image::picker::Capability::Background(red, green, blue) = capability {
                Some(image::Rgba([*red, *green, *blue, 255]))
            } else {
                None
            }
        });
        if let Some(background) = background {
            picker.set_background_color(Some(background));
        }
    }
    picker
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let inner_width = area.width.saturating_sub(4).max(8);
    let wanted =
        wrapped_lines(&app.display_input(), inner_width).clamp(1, MAX_INPUT_ROWS as usize) as u16;
    let refs_height = if app.refs.is_empty() { 0 } else { 5 };
    let palette_height = if app.palette.is_empty() {
        0
    } else {
        app.palette.len().min(5) as u16 + 2
    };
    let reserved = 1 + 6 + 1 + 2 + refs_height + palette_height;
    let max_input = area
        .height
        .saturating_sub(reserved)
        .clamp(1, MAX_INPUT_ROWS);
    let input_height = wanted.min(max_input);

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(refs_height),
        Constraint::Length(palette_height),
        Constraint::Length(input_height + 2),
        Constraint::Length(1),
    ])
    .split(area);

    draw_header(frame, app, chunks[0]);
    draw_stage(frame, app, chunks[1]);
    if refs_height > 0 {
        draw_refs(frame, app, chunks[2]);
    }
    if palette_height > 0 {
        draw_palette(frame, app, chunks[3]);
    }
    draw_input(frame, app, chunks[4]);
    draw_controls(frame, app, chunks[5]);

    if app.help_open {
        draw_help(frame, app, area);
    }
    if app.settings.open {
        draw_settings(frame, app, area);
    }
    if app.finish.is_some() {
        draw_finish(frame, app, area);
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let brand = Line::from(vec![
        Span::styled(
            "FUCKING",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "GEN",
            Style::default()
                .fg(app.accent())
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(brand), area);

    let (color, label) = badge(app.auth_ok, &app.auth_note);
    let badge = Line::from(vec![
        Span::styled(
            "● ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    frame.render_widget(Paragraph::new(badge).alignment(Alignment::Right), area);
}

fn badge(connected: bool, note: &str) -> (Color, String) {
    if connected {
        (Color::Green, format!("CODEX CONNECTED · {note}"))
    } else {
        (Color::Red, format!("CODEX NOT CONNECTED · {note}"))
    }
}

fn draw_stage(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let running = matches!(app.job, Job::Running { .. });
    if !running
        && let Some(item) = app.selected.and_then(|index| app.gallery.get_mut(index))
        && item.protocol.is_none()
        && let Some(image) = item.image.take()
    {
        item.protocol = Some(app.picker.new_resize_protocol(image));
    }
    let title = match app.selected.and_then(|index| app.gallery.get(index)) {
        Some(item) => {
            let marker = if item.saved { "saved" } else { "unsaved" };
            let pending = app.unsaved_count();
            let pending_note = if pending > 1 {
                format!(" · {pending} to decide")
            } else {
                String::new()
            };
            format!(
                " output · {} · {}/{} · {} · {marker}{pending_note} ",
                display_name(&item.path),
                app.selected.unwrap_or(0) + 1,
                app.gallery.len(),
                human_bytes(item.bytes)
            )
        }
        None => " output ".to_string(),
    };
    let border = if running { Color::Yellow } else { app.accent() };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            title,
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if running {
        let (phase, elapsed) = match &app.job {
            Job::Running { started, phase, .. } => (*phase, started.elapsed()),
            Job::Idle => return,
        };
        draw_running_stage(frame, app, inner, &phase, elapsed);
        return;
    }

    if let Some(item) = app.selected.and_then(|index| app.gallery.get_mut(index))
        && let Some(protocol) = item.protocol.as_mut()
    {
        // Sit the image in the middle with breathing room instead of
        // stretching it to the panel edges.
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Fit(None)),
            inner_image_area(inner),
            protocol,
        );
        return;
    }

    app.snake_area = Rect::default();
    app.snake.deactivate();

    let placeholder = vec![
        Line::from(""),
        Line::from(Span::styled("type a prompt, press enter", Style::default())),
        Line::from(Span::styled(
            "drag & drop or paste an image path to attach a reference",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "type / for commands · ↑↓ walks the gallery",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            format!("saves to {}", app.out_dir.display()),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(placeholder).alignment(Alignment::Center),
        inner,
    );
}

fn draw_running_stage(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: Rect,
    phase: &Phase,
    elapsed: Duration,
) {
    // Progress on top, then the biggest square board that fits, centered in
    // whatever is left.
    let progress_height = 5.min(area.height / 2);
    let chunks =
        Layout::vertical([Constraint::Length(progress_height), Constraint::Min(0)]).split(area);
    if snake_board_cells(chunks[1]).is_none() {
        // Too small for a board: just show the progress, centered.
        app.snake_area = Rect::default();
        app.snake.deactivate();
        let lines = marquee_lines(chunks[0].width, app.tick, phase, elapsed, app.accent());
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
        return;
    }
    let lines = marquee_lines(chunks[0].width, app.tick, phase, elapsed, app.accent());
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        chunks[0],
    );
    draw_snake(frame, app, chunks[1]);
}

/// Where a generated image is drawn inside the output panel: centered with a
/// margin, so it neither touches the border nor fills every cell.
fn inner_image_area(area: Rect) -> Rect {
    let margin_x = (area.width / 10).clamp(2, 10);
    let margin_y = (area.height / 12).clamp(1, 4);
    Rect {
        x: area.x.saturating_add(margin_x),
        y: area.y.saturating_add(margin_y),
        width: area.width.saturating_sub(margin_x * 2).max(1),
        height: area.height.saturating_sub(margin_y * 2).max(1),
    }
}

fn draw_snake(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(side) = snake_board_cells(area) else {
        app.snake_area = Rect::default();
        app.snake.deactivate();
        return;
    };

    // Two columns per game cell, so the block is square and sits in the middle.
    let panel = Rect {
        x: area.x + area.width.saturating_sub(side * 2 + 2) / 2,
        y: area.y + area.height.saturating_sub(side + 2) / 2,
        width: side * 2 + 2,
        height: side + 2,
    };
    app.snake_area = panel;
    let focused = app.snake.focused;
    let accent = app.accent();
    let title = if focused {
        format!(
            " snake · {} · arrows to steer · edges wrap ",
            app.snake.score
        )
    } else {
        " snake · paused · arrows or click to play ".to_string()
    };
    let border = if focused { accent } else { Color::DarkGray };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            title,
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    app.snake.resize(side, side);
    let snake = &app.snake;
    let length = snake.body.len();
    let body_style = if focused {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::Gray)
    };
    let mut lines = Vec::with_capacity(usize::from(side));
    for y in 0..side {
        let mut spans = Vec::with_capacity(usize::from(side));
        for x in 0..side {
            let point = SnakePoint { x, y };
            let (glyph, style) = match snake.body.iter().position(|segment| *segment == point) {
                Some(0) => (
                    "● ",
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
                Some(index) => (segment_glyph(index, length), body_style),
                None if snake.food == point => ("◆ ", Style::default().fg(Color::Yellow)),
                None => ("· ", Style::default().fg(Color::DarkGray)),
            };
            spans.push(Span::styled(glyph, style));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn marquee_lines<'a>(
    width: u16,
    tick: u64,
    phase: &Phase,
    elapsed: Duration,
    accent: Color,
) -> Vec<Line<'a>> {
    let spinner = SPINNER[(tick as usize / 2) % SPINNER.len()];
    let phase_text = match phase {
        Phase::Queued => "queued",
        Phase::Generating => "generating",
        Phase::Finishing => "finishing",
    };
    let head = Line::from(vec![
        Span::styled(
            format!("{spinner} {phase_text} "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
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
            ("█", Color::White)
        } else if distance < 4 {
            ("▓", accent)
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
            "the image will pop in here",
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

fn draw_refs(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.hit_refs.clear();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.accent()))
        .title(Span::styled(
            format!(" refs · {} ", app.refs.len()),
            Style::default()
                .fg(app.accent())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut x = inner.x;
    for (index, item) in app.refs.iter_mut().enumerate() {
        if x + 14 > inner.x + inner.width {
            break;
        }
        let cell = Rect {
            x,
            y: inner.y,
            width: 12,
            height: inner.height,
        };
        app.hit_refs.push((cell, index));
        if item.protocol.is_none()
            && let Some(image) = item.image.take()
        {
            item.protocol = Some(app.picker.new_resize_protocol(image));
        }
        if let Some(protocol) = item.protocol.as_mut() {
            frame.render_stateful_widget(
                StatefulImage::new().resize(Resize::Fit(None)),
                cell,
                protocol,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled("▣", Style::default().fg(Color::DarkGray)))
                    .alignment(Alignment::Center),
                cell,
            );
        }
        x += 14;
    }
    let names: Vec<String> = app
        .refs
        .iter()
        .map(|item| display_name(&item.path))
        .collect();
    let hint = if names.is_empty() {
        String::new()
    } else {
        format!("click to remove · {}", names.join(", "))
    };
    let label_area = Rect {
        x: x.min(inner.x + inner.width),
        y: inner.y,
        width: (inner.x + inner.width).saturating_sub(x),
        height: inner.height,
    };
    if label_area.width > 4 {
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray)))
                .wrap(Wrap { trim: true }),
            label_area,
        );
    }
}

fn draw_palette(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.hit_palette.clear();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.accent()))
        .title(Span::styled(
            " commands ",
            Style::default()
                .fg(app.accent())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible = usize::from(inner.height);
    let start = if app.palette_selection >= visible {
        app.palette_selection + 1 - visible
    } else {
        0
    };
    for (row, index) in app.palette.iter().enumerate().skip(start).take(visible) {
        let spec = &COMMANDS[*index];
        let selected = row == app.palette_selection;
        let line_area = Rect {
            x: inner.x,
            y: inner.y + (row - start) as u16,
            width: inner.width,
            height: 1,
        };
        app.hit_palette.push((line_area, row));
        let (name_style, description_style) = if selected {
            (
                Style::default()
                    .fg(Color::Black)
                    .bg(app.accent())
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(Color::Black).bg(app.accent()),
            )
        } else {
            (
                Style::default()
                    .fg(app.accent())
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(Color::DarkGray),
            )
        };
        let args = if spec.args.is_empty() {
            String::new()
        } else {
            format!(" {}", spec.args)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {}{}", spec.name, args), name_style),
                Span::styled(format!("    {}", spec.description), description_style),
            ])),
            line_area,
        );
    }
}

fn draw_input(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.hit_input = area;
    let running = matches!(app.job, Job::Running { .. });
    let focused = app.focus == Focus::Input;
    let border = if running {
        Color::Yellow
    } else if focused {
        app.accent()
    } else {
        Color::DarkGray
    };
    let mut title = String::from(" prompt ");
    if !app.refs.is_empty() {
        title.push_str(&format!("· ▣ {} ", app.refs.len()));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            title,
            Style::default()
                .fg(if focused {
                    app.accent()
                } else {
                    Color::DarkGray
                })
                .add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::horizontal(1));

    let body = if app.input.is_empty() {
        Line::from(Span::styled(
            "Fucking type something…  (/ for commands)",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(app.display_input()).style(Style::default().add_modifier(Modifier::BOLD))
    };
    frame.render_widget(
        Paragraph::new(body)
            .block(block)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::White)),
        area,
    );
}

fn draw_controls(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.hit_button = Rect {
        x: area.x,
        y: area.y,
        width: 14.min(area.width),
        height: 1,
    };
    app.hit_quality = Rect {
        x: area.x + 15,
        y: area.y,
        width: 18.min(area.width.saturating_sub(15)),
        height: 1,
    };
    app.hit_finish_button = Rect {
        x: app.hit_quality.x + app.hit_quality.width + 1,
        y: area.y,
        width: 13.min(
            area.width
                .saturating_sub(app.hit_quality.x + app.hit_quality.width + 1),
        ),
        height: 1,
    };
    let status_x = app.hit_finish_button.x + app.hit_finish_button.width + 1;

    let running = matches!(app.job, Job::Running { .. });
    let label = if running {
        " GENERATING…"
    } else if app.focus == Focus::Button || app.hover == Some(Hit::Button) {
        "[⏎ GENERATE]"
    } else {
        " ⏎ GENERATE "
    };
    let hint_text = if running {
        if app.snake.focused {
            "esc pauses the game · ctrl-c quit "
        } else {
            "esc cancels · ctrl-c finish "
        }
    } else if app.selected.is_some() {
        "↑↓ · enter keep · ctrl-c finish "
    } else {
        "↑↓ · ctrl-c finish "
    };
    let button_style = if running {
        Style::default().fg(Color::DarkGray)
    } else if app.focus == Focus::Button || app.hover == Some(Hit::Button) {
        Style::default()
            .fg(Color::Black)
            .bg(app.accent())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(app.accent())
            .add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(label, button_style))),
        app.hit_button,
    );

    let quality_style = if app.hover == Some(Hit::Quality) {
        Style::default().fg(Color::Black).bg(app.accent())
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" quality: {} ", app.quality),
            quality_style,
        ))),
        app.hit_quality,
    );

    let finish_style = if app.hover == Some(Hit::Finish) {
        Style::default().fg(Color::Black).bg(app.accent())
    } else if app.unsaved_count() > 0 {
        Style::default()
            .fg(app.accent())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(" [finish] ", finish_style))),
        app.hit_finish_button,
    );

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
                Level::Info => Color::DarkGray,
                Level::Ok => Color::Green,
                Level::Err => Color::Red,
            }),
        ),
    };
    if status_x < area.x + area.width {
        let hints_width = u16::try_from(hint_text.chars().count()).unwrap_or(u16::MAX);
        let status_area = Rect {
            x: status_x,
            y: area.y,
            width: (area.width - (status_x - area.x))
                .saturating_sub(hints_width)
                .max(1),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_chars(&left, status_area.width as usize),
                style,
            )),
            status_area,
        );
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint_text,
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Right),
        area,
    );
}

fn draw_settings(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.hit_settings.clear();
    let width = 62.min(area.width);
    let height = (SETTINGS_ROWS as u16 + 4).min(area.height);
    let panel = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, panel);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.accent()))
        .title(Span::styled(
            " settings ",
            Style::default()
                .fg(app.accent())
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " ↑↓ move · ←→ change · enter pick · esc close ",
            Style::default().fg(Color::DarkGray),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let out_dir = app.out_dir.display().to_string();
    let rows = [
        format!("theme        {}", theme_name(app.accent)),
        format!("quality      {}", app.quality),
        format!("save dir     {}", truncate_chars(&out_dir, 34)),
        "login with codex".to_string(),
        "refresh auth status".to_string(),
        "close".to_string(),
    ];
    for (index, label) in rows.iter().enumerate() {
        if usize::from(inner.height) <= index {
            break;
        }
        let row = Rect {
            x: inner.x,
            y: inner.y + index as u16,
            width: inner.width,
            height: 1,
        };
        app.hit_settings.push((row, index));
        let selected = index == app.settings.selection;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(app.accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {label}"), style))),
            row,
        );
    }
}

/// Button row of the finish screen. The second button is honest about what it
/// does: with images already saved this session it drops the rest; with nothing
/// saved yet it throws the lot away.
fn finish_buttons(already_saved: usize) -> [(&'static str, FinishButton); 2] {
    [
        ("keep ticked", FinishButton::KeepTicked),
        (
            if already_saved > 0 {
                "keep already saved"
            } else {
                "save nothing"
            },
            FinishButton::Nothing,
        ),
    ]
}

/// Kitty and iTerm2 carry a real alpha channel, so transparent renders stay
/// transparent there. Everything else gets composited onto the terminal
/// background instead of turning into a black block.
fn protocol_keeps_alpha(protocol: ratatui_image::picker::ProtocolType) -> bool {
    matches!(
        protocol,
        ratatui_image::picker::ProtocolType::Kitty | ratatui_image::picker::ProtocolType::Iterm2
    )
}

/// One file that made it out of the session cache.
struct SavedFile {
    from: PathBuf,
    to: PathBuf,
    prompt: String,
}

struct FinishOutcome {
    saved: Vec<SavedFile>,
    discarded: Vec<PathBuf>,
}

/// Tick what you want to keep: the ticked files move into `out_dir` (with the
/// usual never-overwrite naming) and everything else is deleted.
fn apply_finish(rows: &[FinishRow], out_dir: &Path) -> FinishOutcome {
    let mut outcome = FinishOutcome {
        saved: Vec::new(),
        discarded: Vec::new(),
    };
    for row in rows {
        if row.keep {
            let requested = files::resolve_out_path(None, Some(out_dir), &row.prompt, &row.path);
            if let Ok(target) = files::prepare_path(&requested)
                .and_then(|target| files::move_file(&row.path, &target).map(|()| target))
            {
                outcome.saved.push(SavedFile {
                    from: row.path.clone(),
                    to: target,
                    prompt: row.prompt.clone(),
                });
                continue;
            }
            // A failed move must not turn into a silent delete.
            continue;
        }
        if std::fs::remove_file(&row.path).is_ok() {
            outcome.discarded.push(row.path.clone());
        }
    }
    outcome
}

fn draw_finish(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.hit_finish_rows.clear();
    app.hit_finish_buttons.clear();
    let Some(finish) = app.finish.as_ref() else {
        return;
    };

    let width = 74.min(area.width);
    let rows = finish.rows.len() as u16;
    let height = (rows + 7).min(area.height);
    let panel = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, panel);
    let accent = app.accent();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(
            format!(" finish · keep {}? ", rows),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " space tick · enter keep ticked · n keep nothing · esc stay ",
            Style::default().fg(Color::DarkGray),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let already_saved = app.gallery.iter().filter(|item| item.saved).count();
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            " {} unsaved image(s) · kept ones move to {}",
            rows,
            truncate_chars(&app.out_dir.display().to_string(), 30)
        ),
        Style::default().fg(Color::DarkGray),
    )));
    if already_saved > 0 {
        lines.push(Line::from(Span::styled(
            format!(" {already_saved} already saved this session"),
            Style::default().fg(Color::Green),
        )));
    }
    lines.push(Line::from(""));

    let list_top = inner.y + lines.len() as u16;
    let visible = inner.height.saturating_sub(lines.len() as u16 + 2);
    let start = if finish.selection >= usize::from(visible) {
        finish.selection + 1 - usize::from(visible)
    } else {
        0
    };
    for (index, row) in finish
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(usize::from(visible))
    {
        let selected = index == finish.selection;
        let tick = if row.keep { "[x]" } else { "[ ]" };
        let label = format!(
            " {tick} {} · {} · {}",
            display_name(&row.path),
            human_bytes(row.bytes),
            truncate_chars(&row.prompt.replace('\n', " "), 40)
        );
        let row_area = Rect {
            x: inner.x,
            y: list_top + (index - start) as u16,
            width: inner.width,
            height: 1,
        };
        app.hit_finish_rows.push((row_area, index));
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(app.accent())
                .add_modifier(Modifier::BOLD)
        } else if row.keep {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(Span::styled(label, style)));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    // Buttons along the bottom row of the panel.
    let buttons_y = inner.y + inner.height.saturating_sub(1);
    let mut x = inner.x;
    for (label, button) in finish_buttons(already_saved) {
        let label = format!(" [{label}] ");
        let width = label.chars().count() as u16;
        if x + width > inner.x + inner.width {
            break;
        }
        let rect = Rect {
            x,
            y: buttons_y,
            width,
            height: 1,
        };
        app.hit_finish_buttons.push((rect, button));
        let style = if button == FinishButton::KeepTicked {
            Style::default()
                .fg(Color::Black)
                .bg(app.accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        };
        frame.render_widget(Paragraph::new(Span::styled(label, style)), rect);
        x += width + 1;
    }
}

fn draw_help(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let width = 74.min(area.width);
    let height = 22.min(area.height);
    let panel = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, panel);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.accent()))
        .title(Span::styled(
            " fuckinggen help ",
            Style::default()
                .fg(app.accent())
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " any key closes ",
            Style::default().fg(Color::DarkGray),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let mut lines: Vec<Line> = Vec::new();
    for spec in COMMANDS {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} {}", spec.name, spec.args),
                Style::default()
                    .fg(app.accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   {}", spec.description),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    lines.push(Line::from(""));
    for (key, what) in [
        (
            "enter",
            "generate · run a /command · save the selected image",
        ),
        ("ctrl-c", "keep/discard checklist (again to force quit)"),
        ("shift+enter", "new line, the prompt box grows with it"),
        ("↑ ↓", "walk the generated images"),
        ("arrows", "steer the snake while it generates"),
        ("tab", "focus the generate button"),
        ("esc", "cancel a running generation, clear the prompt"),
        ("ctrl-t", "cycle quality · ctrl-c quit"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {key:<12}"),
                Style::default()
                    .fg(app.accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(what.to_string(), Style::default().fg(Color::White)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn hit_test_static(button: Rect, quality: Rect, input: Rect, column: u16, row: u16) -> Option<Hit> {
    let position = Position::new(column, row);
    if button.contains(position) {
        return Some(Hit::Button);
    }
    if quality.contains(position) {
        return Some(Hit::Quality);
    }
    if input.contains(position) {
        return Some(Hit::Input);
    }
    None
}

/// `dbus-send` arguments for `org.freedesktop.FileManager1.ShowItems`, the
/// Linux implementation of "reveal in the file manager". macOS and Windows
/// have their own reveal commands instead.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn file_manager_args(uri: &str) -> Vec<String> {
    vec![
        "--session".to_string(),
        "--dest=org.freedesktop.FileManager1".to_string(),
        "--type=method_call".to_string(),
        "/org/freedesktop/FileManager1".to_string(),
        "org.freedesktop.FileManager1.ShowItems".to_string(),
        format!("array:string:{uri}"),
        "string:".to_string(),
    ]
}

/// `Some(token)` only when the token really is a command prefix. Absolute paths
/// start with `/` too, and must never be mistaken for commands.
fn command_token(input: &str) -> Option<&str> {
    let token = input.split_whitespace().next()?;
    if !token.starts_with('/') {
        return None;
    }
    COMMANDS
        .iter()
        .any(|spec| spec.name.starts_with(token))
        .then_some(token)
}

fn next_selection(len: usize, current: Option<usize>, delta: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let start = current.unwrap_or(len - 1) as i32;
    let next = (start + delta).clamp(0, len as i32 - 1);
    Some(next as usize)
}

fn palette_matches(input: &str) -> Vec<usize> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') {
        return Vec::new();
    }
    let token = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("/")
        .to_ascii_lowercase();
    if token == "/" {
        return (0..COMMANDS.len()).collect();
    }
    COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, spec)| spec.name.starts_with(&token))
        .map(|(index, _)| index)
        .collect()
}

fn accent_from_config(name: Option<&str>) -> Color {
    let wanted = name.unwrap_or("blue").to_ascii_lowercase();
    THEMES
        .iter()
        .find(|(theme, _)| *theme == wanted)
        .map(|(_, color)| *color)
        .unwrap_or(Color::Blue)
}

fn theme_name(color: Color) -> &'static str {
    THEMES
        .iter()
        .find(|(_, candidate)| *candidate == color)
        .map(|(name, _)| *name)
        .unwrap_or("blue")
}

fn is_newline_modifier(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::SUPER)
}

pub fn run_tui(args: TuiArgs) -> Result<i32> {
    let mut app = App::new(args)?;
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    // Click-only mouse tracking (X10 + SGR). Any-motion mode is deliberately
    // left off: with it enabled macOS terminals route a Finder drag to the app
    // as mouse events and the dropped file path never arrives.
    let _ = std::io::stdout().write_all(b"\x1b[?1000h\x1b[?1006h");
    let _ = std::io::stdout().flush();
    let result = app.run(&mut terminal);
    let _ = std::io::stdout().write_all(b"\x1b[?1006l\x1b[?1000l");
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result?;
    report_session(&app);
    Ok(0)
}

/// What to tell the user after the TUI closes: a hard quit keeps the staged
/// renders (losing a picture you paid a generation for is worse than a cache
/// directory), a clean exit leaves nothing behind.
fn session_report(forced_quit: bool, staged: usize, session_dir: &Path) -> Option<String> {
    if staged == 0 {
        let _ = std::fs::remove_dir(session_dir);
        return None;
    }
    if forced_quit {
        return Some(format!(
            "left {staged} unsaved image(s) in {}",
            session_dir.display()
        ));
    }
    None
}

fn report_session(app: &App) {
    let staged = app.unsaved_count();
    if let Some(message) = session_report(app.forced_quit, staged, &app.session_dir) {
        eprintln!("{message}");
    }
}

fn is_image_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif"]
        .iter()
        .any(|extension| name.ends_with(extension))
        && path.is_file()
}

/// Turn dropped or typed text into image paths. Covers what Finder drags look
/// like in macOS terminals: backslash-escaped spaces, single or double quotes,
/// `file://` URLs, and multi-file drops separated by newlines.
fn drop_paths(text: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for raw_line in text.split(['\n', '\r']) {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let unquoted = line.trim_matches(|c| c == '\'' || c == '"');
        let unescaped = unquoted
            .replace("\\ ", " ")
            .replace("\\'", "'")
            .replace("\\\"", "\"");
        let candidate = unescaped
            .strip_prefix("file://")
            .unwrap_or(unescaped.as_str());
        let path = PathBuf::from(files::expand_tilde(
            candidate.trim_matches(|c| c == '\'' || c == '"'),
        ));
        if is_image_file(&path) && !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// True when the whole input is nothing but image paths.
fn input_paths(input: &str) -> Option<Vec<PathBuf>> {
    let lines = input
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .count();
    if lines == 0 {
        return None;
    }
    let paths = drop_paths(input);
    (paths.len() == lines).then_some(paths)
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
    fn drop_paths_handles_every_terminal_flavour() {
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("ref image.png");
        std::fs::write(&image, [0x89, b'P', b'N', b'G']).unwrap();
        let raw = image.to_str().unwrap();

        assert_eq!(drop_paths(raw), vec![image.clone()]);
        assert_eq!(drop_paths(&raw.replace(' ', "\\ ")), vec![image.clone()]);
        assert_eq!(drop_paths(&format!("'{raw}'")), vec![image.clone()]);
        assert_eq!(drop_paths(&format!("\"{raw}\"")), vec![image.clone()]);
        assert_eq!(drop_paths(&format!("file://{raw}")), vec![image.clone()]);

        let second = tmp.path().join("another.png");
        std::fs::write(&second, [0x89, b'P', b'N', b'G']).unwrap();
        assert_eq!(
            drop_paths(&format!("{raw}\n{}", second.display())),
            vec![image.clone(), second.clone()]
        );

        let text_file = tmp.path().join("notes.txt");
        std::fs::write(&text_file, b"hello").unwrap();
        assert!(drop_paths(text_file.to_str().unwrap()).is_empty());
        assert!(drop_paths("just some text").is_empty());
    }

    #[test]
    fn input_paths_only_promotes_pure_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("cat.png");
        std::fs::write(&image, [0x89, b'P', b'N', b'G']).unwrap();
        assert_eq!(
            input_paths(image.to_str().unwrap()),
            Some(vec![image.clone()])
        );
        assert_eq!(
            input_paths(&format!("make it blue {}", image.display())),
            None
        );
        assert_eq!(input_paths(""), None);
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

    #[test]
    fn badge_is_green_when_connected_and_red_when_not() {
        let (color, label) = badge(true, "abd602");
        assert_eq!(color, Color::Green);
        assert!(label.contains("CONNECTED"));
        assert!(!label.contains("NOT"));
        let (color, label) = badge(false, "run codex login");
        assert_eq!(color, Color::Red);
        assert!(label.contains("NOT CONNECTED"));
    }

    #[test]
    fn hit_test_priorities() {
        let button = Rect::new(0, 20, 14, 1);
        let quality = Rect::new(15, 20, 18, 1);
        let input = Rect::new(0, 10, 80, 8);
        assert_eq!(
            hit_test_static(button, quality, input, 3, 20),
            Some(Hit::Button)
        );
        assert_eq!(
            hit_test_static(button, quality, input, 16, 20),
            Some(Hit::Quality)
        );
        assert_eq!(
            hit_test_static(button, quality, input, 40, 12),
            Some(Hit::Input)
        );
        assert_eq!(hit_test_static(button, quality, input, 40, 5), None);
    }

    #[test]
    fn newline_modifiers_are_detected() {
        assert!(is_newline_modifier(KeyModifiers::SHIFT));
        assert!(is_newline_modifier(KeyModifiers::ALT));
        assert!(is_newline_modifier(KeyModifiers::SUPER));
        assert!(!is_newline_modifier(KeyModifiers::NONE));
        assert!(!is_newline_modifier(KeyModifiers::CONTROL));
    }

    #[test]
    fn focus_toggles() {
        assert_eq!(Focus::Input.toggled(), Focus::Button);
        assert_eq!(Focus::Button.toggled(), Focus::Input);
    }

    #[test]
    fn a_forced_quit_keeps_staged_images_and_says_where() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session-1");
        std::fs::create_dir(&session).unwrap();

        // Clean exit with nothing staged: cache directory goes away, no message.
        assert_eq!(session_report(false, 0, &session), None);
        assert!(!session.exists());

        // Forced quit with staged work: keep the files and say where they are.
        std::fs::create_dir(&session).unwrap();
        let message = session_report(true, 3, &session).unwrap();
        assert!(message.contains("3 unsaved"));
        assert!(message.contains(&session.display().to_string()));
        assert!(session.exists(), "staged renders survive a hard quit");
    }

    #[test]
    fn finish_buttons_name_what_they_do() {
        let fresh = finish_buttons(0);
        assert_eq!(fresh[0].0, "keep ticked");
        assert_eq!(fresh[1].0, "save nothing");
        let after_saving = finish_buttons(3);
        assert_eq!(after_saving[1].0, "keep already saved");
    }

    #[test]
    fn alpha_is_kept_only_where_the_protocol_can() {
        use ratatui_image::picker::ProtocolType;
        assert!(protocol_keeps_alpha(ProtocolType::Kitty));
        assert!(protocol_keeps_alpha(ProtocolType::Iterm2));
        assert!(!protocol_keeps_alpha(ProtocolType::Halfblocks));
        assert!(!protocol_keeps_alpha(ProtocolType::Sixel));
    }

    #[test]
    fn turning_steps_immediately() {
        let mut game = SnakeGame::new();
        game.resize(12, 12);
        game.focused = true;
        let head = game.body[0];
        game.set_direction(SnakeDirection::Up);
        // No sleeping: a turn must move the snake on the very next tick.
        game.tick();
        assert_eq!(
            game.body[0],
            SnakePoint {
                x: head.x,
                y: head.y - 1
            }
        );
    }

    #[test]
    fn finish_checklist_moves_ticked_and_deletes_the_rest() {
        let staged = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let make = |name: &str, body: &[u8]| {
            let path = staged.path().join(name);
            std::fs::write(&path, body).unwrap();
            path
        };
        let keep = make("keep-me.png", b"kept");
        let drop = make("drop-me.png", b"dropped");
        // Same derived name as an existing file: the kept one must not clobber it.
        let taken = out.path().join("taken.png");
        std::fs::write(&taken, b"original").unwrap();
        let collides = make("collides.png", b"second");

        let rows = vec![
            FinishRow {
                path: keep.clone(),
                prompt: "taken".to_string(),
                bytes: 4,
                keep: true,
            },
            FinishRow {
                path: collides.clone(),
                prompt: "taken".to_string(),
                bytes: 6,
                keep: true,
            },
            FinishRow {
                path: drop.clone(),
                prompt: "drop-me".to_string(),
                bytes: 7,
                keep: false,
            },
        ];
        let outcome = apply_finish(&rows, out.path());

        assert_eq!(outcome.saved.len(), 2);
        assert_eq!(outcome.discarded, vec![drop.clone()]);
        assert!(!keep.exists(), "kept file moved out of the cache");
        assert!(!collides.exists());
        assert!(!drop.exists(), "unticked file deleted");
        assert!(staged.path().read_dir().unwrap().next().is_none());
        assert_eq!(std::fs::read(&taken).unwrap(), b"original");
        // The occupied name was stepped around for both, never overwritten.
        let names: Vec<String> = outcome
            .saved
            .iter()
            .map(|saved| saved.to.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["taken-v2.png", "taken-v3.png"]);
    }

    #[test]
    fn unsaved_gallery_items_become_ticked_checklist_rows() {
        let item = |name: &str, saved: bool| GalleryItem {
            path: PathBuf::from(name),
            prompt: name.to_string(),
            bytes: 1,
            saved,
            image: None,
            protocol: None,
        };
        let gallery = [
            item("one.png", false),
            item("two.png", true),
            item("three.png", false),
        ];
        let rows: Vec<FinishRow> = gallery
            .iter()
            .filter(|item| !item.saved)
            .map(|item| FinishRow {
                path: item.path.clone(),
                prompt: item.prompt.clone(),
                bytes: item.bytes,
                keep: true,
            })
            .collect();
        assert_eq!(rows.len(), 2, "already saved images are not asked about");
        assert!(
            rows.iter().all(|row| row.keep),
            "everything is ticked by default"
        );
        assert_eq!(rows[0].path, PathBuf::from("one.png"));
        assert_eq!(rows[1].path, PathBuf::from("three.png"));
    }

    #[test]
    fn snake_board_is_square_and_fits() {
        // 80x30 panel: rows are the limit (30 - 2 borders), and the board is square.
        assert_eq!(snake_board_cells(Rect::new(0, 0, 80, 30)), Some(28));
        // Narrow panel: columns become the limit ((40 - 2) / 2 = 19).
        assert_eq!(snake_board_cells(Rect::new(0, 0, 40, 30)), Some(19));
        // The board never exceeds the cap, even in a huge terminal.
        assert_eq!(snake_board_cells(Rect::new(0, 0, 400, 200)), Some(28));
        // Too small to play: no board at all.
        assert_eq!(snake_board_cells(Rect::new(0, 0, 20, 8)), None);
    }

    #[test]
    fn snake_body_tapers_towards_the_tail() {
        let length = 12;
        let glyphs: Vec<&str> = (0..length).map(|i| segment_glyph(i, length)).collect();
        assert_eq!(glyphs[0], "●", "head is a full circle");
        assert_eq!(glyphs[length - 1], ".", "tail is the smallest dot");
        // Sizes must never grow again towards the tail.
        let rank = |g: &str| match g {
            "●" => 4,
            "•" => 3,
            "∙" => 2,
            "·" => 1,
            _ => 0,
        };
        assert!(glyphs.windows(2).all(|pair| rank(pair[0]) >= rank(pair[1])));
        // The ramp is graded, not three chunky bands.
        let distinct: std::collections::BTreeSet<&&str> = glyphs.iter().collect();
        assert!(
            distinct.len() >= 4,
            "expected a graded taper, got {distinct:?}"
        );
        // Short snakes stay solid.
        assert_eq!(segment_glyph(2, 3), "●");
        assert_eq!(segment_glyph(1, 0), "●");
    }

    #[test]
    fn snake_wraps_at_every_board_edge() {
        let mut game = SnakeGame::new();
        game.resize(8, 4);
        game.focused = true;
        game.body = vec![SnakePoint { x: 0, y: 1 }, SnakePoint { x: 1, y: 1 }];
        game.food = SnakePoint { x: 4, y: 3 };
        game.direction = SnakeDirection::Left;
        game.step();
        assert_eq!(game.body[0], SnakePoint { x: 7, y: 1 });

        game.body = vec![SnakePoint { x: 2, y: 0 }, SnakePoint { x: 2, y: 1 }];
        game.food = SnakePoint { x: 4, y: 3 };
        game.direction = SnakeDirection::Up;
        game.step();
        assert_eq!(game.body[0], SnakePoint { x: 2, y: 3 });
    }

    #[test]
    fn snake_rejects_immediate_reverse() {
        let mut game = SnakeGame::new();
        game.resize(8, 4);
        game.focused = true;
        game.body = vec![SnakePoint { x: 3, y: 1 }, SnakePoint { x: 2, y: 1 }];
        game.food = SnakePoint { x: 7, y: 3 };
        game.direction = SnakeDirection::Right;
        game.set_direction(SnakeDirection::Left);
        game.step();
        assert_eq!(game.body[0], SnakePoint { x: 4, y: 1 });
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn file_manager_args_reveal_the_file() {
        let args = file_manager_args("file:///tmp/out/img.png");
        assert_eq!(args[1], "--dest=org.freedesktop.FileManager1");
        assert_eq!(args[4], "org.freedesktop.FileManager1.ShowItems");
        assert_eq!(args[5], "array:string:file:///tmp/out/img.png");
    }

    #[test]
    fn command_token_ignores_paths() {
        assert_eq!(command_token("/"), Some("/"));
        assert_eq!(command_token("/open"), Some("/open"));
        assert_eq!(command_token("/remov"), Some("/remov"));
        assert_eq!(command_token("/quality low"), Some("/quality"));
        assert_eq!(command_token("/tmp/drop\\ test/tabby.png"), None);
        assert_eq!(command_token("/Users/me/image.png"), None);
        assert_eq!(command_token("/frobnicate"), None);
        assert_eq!(command_token("make it blue"), None);
    }

    #[test]
    fn gallery_selection_walks_and_clamps() {
        assert_eq!(next_selection(0, None, -1), None);
        // no selection starts from the newest item, then moves and clamps
        assert_eq!(next_selection(3, None, -1), Some(1));
        assert_eq!(next_selection(3, None, 1), Some(2));
        assert_eq!(next_selection(3, None, 0), Some(2));
        assert_eq!(next_selection(3, Some(2), -1), Some(1));
        assert_eq!(next_selection(3, Some(1), -1), Some(0));
        assert_eq!(next_selection(3, Some(0), -1), Some(0));
        assert_eq!(next_selection(3, Some(0), 1), Some(1));
        assert_eq!(next_selection(3, Some(2), 1), Some(2));
    }

    #[test]
    fn palette_matches_commands() {
        assert!(palette_matches("hello").is_empty());
        assert_eq!(palette_matches("/").len(), COMMANDS.len());
        let matches = palette_matches("/rem");
        assert_eq!(matches.len(), 2);
        assert_eq!(COMMANDS[matches[0]].name, "/remove");
        assert_eq!(COMMANDS[matches[1]].name, "/remove-all");
        assert_eq!(palette_matches("/roo").len(), 1);
        assert!(palette_matches("/nope").is_empty());
    }

    #[test]
    fn accent_defaults_to_blue_and_cycles() {
        assert_eq!(accent_from_config(None), Color::Blue);
        assert_eq!(accent_from_config(Some("cyan")), Color::Cyan);
        assert_eq!(accent_from_config(Some("nonsense")), Color::Blue);
        assert_eq!(theme_name(Color::Blue), "blue");
        assert_eq!(theme_name(Color::Cyan), "cyan");
    }
}
