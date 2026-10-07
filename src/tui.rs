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
        name: "/history",
        args: "",
        description: "everything you kept, ready to iterate on again",
    },
    CommandSpec {
        name: "/guide",
        args: "",
        description: "show the tour again",
    },
    CommandSpec {
        name: "/new",
        args: "",
        description: "empty the prompt for a fresh idea",
    },
    CommandSpec {
        name: "/regen",
        args: "",
        description: "another take on the selected image's prompt",
    },
    CommandSpec {
        name: "/bg",
        args: "",
        description: "same picture, background removed (transparent)",
    },
    CommandSpec {
        name: "/copy",
        args: "",
        description: "copy the selected image's prompt",
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
    NewPrompt,
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
    /// Adopted from an earlier session: decode it the first time it is shown.
    needs_load: bool,
    image: Option<image::DynamicImage>,
    protocol: Option<StatefulProtocol>,
}

/// One staged image in the "what do we keep" screen shown on the way out.
struct FinishRow {
    path: PathBuf,
    prompt: String,
    bytes: usize,
    keep: bool,
    /// Its own small protocol: the grid already has one, and sharing a stateful
    /// protocol between two differently sized render sites makes it thrash.
    thumb: Option<StatefulProtocol>,
}

/// One line of the history list: a picture that was kept, somewhere on disk.
struct HistoryRow {
    path: PathBuf,
    prompt: String,
    thumb: Option<StatefulProtocol>,
}

/// Everything kept so far, so a picture from last week is one keypress away.
struct History {
    rows: Vec<HistoryRow>,
    selection: usize,
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

const SETTINGS_ROWS: usize = 7;

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
/// Shown next to the best run of the session board.
const FIRE: &str = "🔥";

/// Nerd Font icons, so the controls read at a glance. A terminal without a
/// patched font shows boxes; the words next to every icon still say it.
const ICON_BOLT: &str = "\u{f0e7}"; // generate
const ICON_CHECK: &str = "\u{f00c}"; // keep / finish
const ICON_PLUS: &str = "\u{f067}"; // new prompt
const ICON_SLIDERS: &str = "\u{f1de}"; // quality
const ICON_HISTORY: &str = "\u{f1da}"; // history
const ICON_TRASH: &str = "\u{f1f8}"; // remove
const ICON_REFRESH: &str = "\u{f021}"; // another take
const ICON_COPY: &str = "\u{f0c5}"; // copy prompt
const ICON_CROP: &str = "\u{f125}"; // cut-out
const ICON_IMAGE: &str = "\u{f03e}"; // gallery
const ICON_LEFT: &str = "\u{f060}";
const ICON_RIGHT: &str = "\u{f061}";
const ICON_ZOOM: &str = "\u{f00e}"; // magnifier
/// The board is a square block in the middle of the output panel, kept compact
/// so it reads as a diversion rather than a takeover.
const MIN_SNAKE_SIDE: u16 = 6;
const MAX_SNAKE_SIDE: u16 = 14;

/// Biggest square board (in game cells) that fits in `area`. Every game cell
/// takes two terminal columns, so a square cell grid looks square on screen.
/// `None` when there is no room for a game worth playing.
fn snake_board_cells(area: Rect) -> Option<u16> {
    let columns = area.width.saturating_sub(2) / 2;
    let rows = area.height.saturating_sub(2);
    let side = columns.min(rows).min(MAX_SNAKE_SIDE);
    (side >= MIN_SNAKE_SIDE).then_some(side)
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
    /// Best run this install, persisted in the config.
    high_score: u32,
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
            high_score: 0,
        }
    }

    /// A finished generation starts a fresh board.
    fn restart(&mut self) {
        self.reset();
        self.place_food();
        self.last_step = Instant::now();
    }

    /// Returns true when the run just beat the stored best.
    fn award(&mut self) -> bool {
        if self.score > self.high_score {
            self.high_score = self.score;
            return true;
        }
        false
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
    hit_new_button: Rect,
    hit_palette: Vec<(Rect, usize)>,
    hit_refs: Vec<(Rect, usize)>,
    hit_settings: Vec<(Rect, usize)>,
    snake_area: Rect,
    palette: Vec<usize>,
    palette_selection: usize,
    settings: Settings,
    /// The prompt of the newest generation, shown in the terminal title when
    /// the prompt box is empty.
    last_prompt: String,
    /// When the checklist opened, so a keystroke arriving in the same instant
    /// (key repeat, or the keystroke that opened it) cannot answer for you.
    finish_opened: Option<Instant>,
    /// Last title we pushed to the terminal, so we only rewrite on change.
    title_cache: String,
    /// Scratch directory for generations the user has not decided about yet.
    session_dir: PathBuf,
    finish: Option<Finish>,
    forced_quit: bool,
    /// Follow-up generations hand the selected image back to the model, since
    /// the backend request is stateless. `/ctx` turns it off.
    carry_context: bool,
    /// The `+` cell of the grid is selected instead of an image.
    grid_on_plus: bool,
    /// Full-screen image view: zoom factor and pan, in source pixels.
    viewer: Option<Viewer>,
    /// Previously kept pictures, newest first, opened with /history.
    history: Option<History>,
    /// Cell rectangles of the grid, for clicking.
    hit_grid: Vec<(Rect, usize)>,
    hit_history: Vec<(Rect, usize)>,
    /// Columns the grid was drawn with, so row moves match what is on screen.
    grid_columns: usize,
    /// True while the user is walking the grid with the arrow keys: single
    /// letters then act as shortcuts instead of typing.
    browse_mode: bool,
    /// Set as soon as the user edits the prompt, so browsing never eats a draft.
    prompt_dirty: bool,
    /// First-run tour page, or `None` when it is not on screen.
    intro: Option<usize>,
    /// Coffee popup auto-close deadline, while it is on screen.
    coffee_until: Option<Instant>,
    /// Generations produced by this install (persisted in the config).
    runs: u32,
    hit_intro: Rect,
    hit_coffee: Rect,
    hit_finish_rows: Vec<(Rect, usize)>,
    hit_finish_buttons: Vec<(Rect, FinishButton)>,
    help_open: bool,
    /// First line of the help panel, so long help can be scrolled.
    help_scroll: usize,
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
        // Read these before `config` moves into the app.
        let intro_page = if config.intro_seen.unwrap_or(false) {
            None
        } else {
            Some(0)
        };
        let runs = config.runs.unwrap_or(0);
        let snake_high = config.snake_high.unwrap_or(0);
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
        // A session starts empty: leftovers from earlier sessions are scratch,
        // and anything still unsaved after a day is deleted.
        let pruned = files::prune_stale_sessions(&session_dir);
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
            hit_new_button: Rect::default(),
            hit_palette: Vec::new(),
            hit_refs: Vec::new(),
            hit_settings: Vec::new(),
            snake_area: Rect::default(),
            palette: Vec::new(),
            palette_selection: 0,
            settings: Settings::default(),
            last_prompt: String::new(),
            finish_opened: None,
            title_cache: String::new(),
            session_dir: session_dir.clone(),
            finish: None,
            forced_quit: false,
            carry_context: true,
            browse_mode: false,
            grid_on_plus: false,
            viewer: None,
            history: None,
            hit_grid: Vec::new(),
            hit_history: Vec::new(),
            grid_columns: 1,
            prompt_dirty: false,
            intro: intro_page,
            coffee_until: None,
            runs,
            hit_intro: Rect::default(),
            hit_coffee: Rect::default(),
            hit_finish_rows: Vec::new(),
            hit_finish_buttons: Vec::new(),
            help_open: false,
            help_scroll: 0,
            auth_ok: false,
            auth_note: String::new(),
            quit: false,
            pending_login: false,
        };
        if pruned > 0 {
            app.set_status(
                Level::Info,
                format!("cleaned up {pruned} unsaved image(s) older than a day"),
            );
        }
        app.snake.high_score = snake_high;
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
            self.coffee_tick();
            self.sync_terminal_title();
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

    /// Open the archive of kept pictures (state file, newest first).
    fn open_history(&mut self) {
        let rows: Vec<HistoryRow> = state::recent(50)
            .into_iter()
            .map(|record| (PathBuf::from(&record.path), record.prompt))
            .filter(|(path, _)| path.is_file())
            .map(|(path, prompt)| HistoryRow {
                path,
                prompt,
                thumb: None,
            })
            .collect();
        if rows.is_empty() {
            self.set_status(Level::Info, "nothing kept yet — save something first");
            return;
        }
        self.history = Some(History { rows, selection: 0 });
        self.set_status(Level::Info, "history · enter loads it into the grid");
    }

    /// Pull a kept picture back into the session so it can be iterated on.
    fn history_load(&mut self) {
        let Some(history) = self.history.take() else {
            return;
        };
        let Some(row) = history.rows.get(history.selection) else {
            return;
        };
        let path = row.path.clone();
        let prompt = row.prompt.clone();
        if self.gallery.iter().any(|item| item.path == path) {
            self.selected = self
                .gallery
                .iter()
                .position(|item| item.path == path)
                .or(self.selected);
            self.grid_on_plus = false;
            let name = display_name(&path);
            self.set_status(Level::Info, format!("{name} is already in the grid"));
            return;
        }
        let bytes = std::fs::metadata(&path)
            .map(|meta| meta.len() as usize)
            .unwrap_or(0);
        self.gallery.push(GalleryItem {
            path: path.clone(),
            prompt,
            bytes,
            saved: true,
            needs_load: true,
            image: None,
            protocol: None,
        });
        self.selected = Some(self.gallery.len() - 1);
        self.grid_on_plus = false;
        self.load_selected_prompt();
        let name = display_name(&path);
        self.set_status(
            Level::Ok,
            format!("{name} back in the grid — edit the prompt and press enter"),
        );
    }

    fn on_history_key(&mut self, key: KeyEvent) {
        let Some(history) = self.history.as_mut() else {
            return;
        };
        let len = history.rows.len();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                history.selection = (history.selection + 1) % len;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                history.selection = (history.selection + len - 1) % len;
            }
            KeyCode::PageDown => {
                history.selection = (history.selection + 8).min(len - 1);
            }
            KeyCode::PageUp => history.selection = history.selection.saturating_sub(8),
            KeyCode::Enter => self.history_load(),
            KeyCode::Esc | KeyCode::Char('q') => {
                self.history = None;
                self.set_status(Level::Info, "back to the grid");
            }
            _ => {}
        }
    }

    /// Move to the next picture without leaving the full-screen view.
    fn viewer_step_image(&mut self, delta: i32) {
        let Some(current) = self.selected else {
            return;
        };
        let next = next_selection(self.gallery.len(), Some(current), delta);
        if next == Some(current) {
            return;
        }
        self.selected = next;
        self.grid_on_plus = false;
        self.open_viewer();
        if let Some(item) = next.and_then(|index| self.gallery.get(index)) {
            let name = display_name(&item.path);
            self.set_status(Level::Info, format!("{name} · ← → image · + zoom"));
        }
    }

    fn set_status(&mut self, level: Level, text: impl Into<String>) {
        self.level = level;
        self.status = text.into();
    }

    /// The terminal tab says what this window is doing: the prompt you typed,
    /// the prompt being rendered (with the spinner), or the checklist question.
    fn sync_terminal_title(&mut self) {
        let running = matches!(self.job, Job::Running { .. });
        let spinner = SPINNER[(self.tick as usize / 2) % SPINNER.len()];
        let prompt = self
            .selected
            .and_then(|index| self.gallery.get(index))
            .map(|item| item.prompt.as_str())
            .filter(|prompt| !prompt.trim().is_empty())
            .unwrap_or(self.last_prompt.as_str());
        let title = window_title(
            spinner,
            running,
            prompt,
            &self.input,
            self.finish.as_ref().map(|finish| finish.rows.len()),
        );
        if title == self.title_cache {
            return;
        }
        self.title_cache = title.clone();
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "{}", title_escape(&title));
        let _ = stdout.flush();
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
        if self.hit_new_button.contains(position) {
            return Some(Hit::NewPrompt);
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
        if self.coffee_until.is_some() {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.dismiss_coffee(true);
            }
            return;
        }
        if self.intro.is_some() {
            if matches!(mouse.kind, MouseEventKind::Down(_))
                && let Some(page) = self.intro
            {
                if page + 1 >= INTRO_PAGES.len() {
                    self.finish_intro();
                } else {
                    self.intro = Some(page + 1);
                }
            }
            return;
        }
        if self.viewer.is_some() {
            return;
        }
        if self.finish.is_some() {
            self.on_finish_mouse(mouse);
            return;
        }
        // Clicking a grid cell selects it; clicking it again opens the viewer.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            for (rect, index) in &self.hit_grid {
                if rect.contains(Position::new(mouse.column, mouse.row)) {
                    let index = *index;
                    let already =
                        self.browse_mode && !self.grid_on_plus && self.selected == Some(index);
                    self.browse_mode = true;
                    self.grid_set_cursor(index);
                    if already {
                        self.open_viewer();
                    }
                    return;
                }
            }
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
                    Some(Hit::NewPrompt) => self.new_prompt(),
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
            // Scrollable help: everyone who wants out presses esc or q.
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(8),
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(8),
                KeyCode::Home => self.help_scroll = 0,
                KeyCode::End => self.help_scroll = usize::MAX / 2,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.help_open = false;
                    self.help_scroll = 0;
                }
                _ => {}
            }
            return;
        }
        if self.settings.open {
            self.on_settings_key(key);
            return;
        }
        if self.coffee_until.is_some() {
            match key.code {
                KeyCode::Enter => self.dismiss_coffee(true),
                _ => self.dismiss_coffee(false),
            }
            return;
        }
        if self.history.is_some() {
            self.on_history_key(key);
            return;
        }
        if self.viewer.is_some() {
            let mut step_image = 0i32;
            {
                let viewer = self.viewer.as_mut().expect("checked above");
                let zoomed = viewer.zoom > 1.0;
                match key.code {
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        viewer.zoom = (viewer.zoom * 1.25).min(8.0);
                    }
                    KeyCode::Char('-') | KeyCode::Char('_') => {
                        viewer.zoom = (viewer.zoom / 1.25).max(1.0);
                    }
                    // Zoomed in, the arrows walk around the picture. At fit size
                    // there is nothing to pan, so left/right change pictures
                    // instead of doing nothing at all.
                    KeyCode::Left | KeyCode::Char('h') if zoomed => {
                        viewer.pan_x = (viewer.pan_x - 0.08).clamp(0.0, 1.0);
                    }
                    KeyCode::Right | KeyCode::Char('l') if zoomed => {
                        viewer.pan_x = (viewer.pan_x + 0.08).clamp(0.0, 1.0);
                    }
                    KeyCode::Up | KeyCode::Char('k') if zoomed => {
                        viewer.pan_y = (viewer.pan_y - 0.08).clamp(0.0, 1.0);
                    }
                    KeyCode::Down | KeyCode::Char('j') if zoomed => {
                        viewer.pan_y = (viewer.pan_y + 0.08).clamp(0.0, 1.0);
                    }
                    KeyCode::Left | KeyCode::Char('h') | KeyCode::PageUp => step_image = -1,
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::PageDown => step_image = 1,
                    KeyCode::Char(' ') | KeyCode::Esc | KeyCode::Char('q') => {
                        self.viewer = None;
                        self.set_status(Level::Info, "back to the grid");
                    }
                    _ => {}
                }
            }
            if step_image != 0 {
                self.viewer_step_image(step_image);
            }
            return;
        }
        if self.intro.is_some() {
            if matches!(
                (key.code, key.modifiers),
                (KeyCode::Char('c'), KeyModifiers::CONTROL)
            ) {
                self.finish_intro();
                return;
            }
            self.on_intro_key(key);
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
            (KeyCode::Esc, _) if self.browse_mode => self.leave_grid(),
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
                    self.prompt_dirty = false;
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
            // Up enters the grid (or walks a row up); down walks a row down and
            // then hands the prompt back.
            (KeyCode::Up, _) if self.browse_mode => {
                let columns = self.grid_columns;
                self.grid_step_row(-1, columns);
            }
            (KeyCode::Down, _) if self.browse_mode => {
                let columns = self.grid_columns;
                if !self.grid_step_row(1, columns) {
                    self.leave_grid();
                }
            }
            (KeyCode::Left, _) if self.browse_mode => self.grid_step(-1),
            (KeyCode::Right, _) if self.browse_mode => self.grid_step(1),
            (KeyCode::Up, _) => self.enter_grid(),
            (KeyCode::Down, _) => self.enter_grid(),
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
                } else if self.grid_on_plus && self.browse_mode && !self.input.trim().is_empty() {
                    // The + cell is the "make something new" square.
                    self.submit();
                } else if self.input.trim().is_empty()
                    && self.selected.is_some()
                    && !self.browse_mode
                {
                    // Enter on an empty prompt keeps the image you are looking at.
                    self.save_selected();
                } else {
                    self.submit();
                }
            }
            (KeyCode::Char(' '), KeyModifiers::NONE)
                if self.input.trim().is_empty()
                    && self.selected.is_some()
                    && !self.grid_on_plus =>
            {
                self.open_viewer();
            }
            (KeyCode::Char(' '), KeyModifiers::NONE)
                if self.focus == Focus::Button && self.input.is_empty() =>
            {
                self.submit()
            }
            // While walking the gallery, single keys act on the picture you are
            // looking at instead of typing into the box.
            (code, KeyModifiers::NONE)
                if browse_shortcut(
                    code,
                    self.browse_mode,
                    self.prompt_dirty,
                    self.selected.is_some(),
                    self.input.is_empty(),
                )
                .is_some() =>
            {
                match browse_shortcut(
                    code,
                    self.browse_mode,
                    self.prompt_dirty,
                    self.selected.is_some(),
                    self.input.is_empty(),
                ) {
                    Some(BrowseAction::Remove) => self.remove_selected(),
                    Some(BrowseAction::Save) => self.save_selected(),
                    Some(BrowseAction::History) => self.open_history(),
                    Some(BrowseAction::Regenerate) => self.regenerate_selected(),
                    Some(BrowseAction::Cutout) => self.remove_background(),
                    Some(BrowseAction::CopyPrompt) => self.copy_selected_prompt(),
                    Some(BrowseAction::NewPrompt) => self.new_prompt(),
                    None => {}
                }
            }
            (KeyCode::Backspace, _) => {
                self.focus = Focus::Input;
                self.prompt_dirty = true;
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
                self.browse_mode = false;
                self.prompt_dirty = true;
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
                let enabled = !self.config.coffee_enabled.unwrap_or(true);
                self.config.coffee_enabled = Some(enabled);
                self.save_config();
                let state = if enabled { "on" } else { "off" };
                self.set_status(Level::Info, format!("coffee popup {state}"));
            }
            4 => {
                self.settings.open = false;
                self.pending_login = true;
            }
            5 => {
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
            "/history" => self.open_history(),
            "/guide" => self.intro = Some(0),
            "/help" => {
                self.help_open = true;
                self.help_scroll = 0;
            }
            "/new" => self.new_prompt(),
            "/regen" => self.regenerate_selected(),
            "/bg" => self.remove_background(),
            "/copy" => self.copy_selected_prompt(),
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
                thumb: None,
            })
            .collect();
        if rows.is_empty() {
            self.quit = true;
            return;
        }
        self.finish = Some(Finish { rows, selection: 0 });
        self.finish_opened = Some(Instant::now());
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

    /// The first-run tour: pages of plain talk about what this thing does.
    fn on_intro_key(&mut self, key: KeyEvent) {
        let Some(page) = self.intro else {
            return;
        };
        let last = INTRO_PAGES.len() - 1;
        match (key.code, key.modifiers) {
            (KeyCode::Right, _) | (KeyCode::Enter, _) | (KeyCode::Char(' '), _) => {
                if page >= last {
                    self.finish_intro();
                } else {
                    self.intro = Some(page + 1);
                }
            }
            (KeyCode::Left, _) => self.intro = Some(page.saturating_sub(1)),
            (KeyCode::Esc, _) => self.finish_intro(),
            _ => {}
        }
    }

    fn finish_intro(&mut self) {
        self.intro = None;
        self.config.intro_seen = Some(true);
        self.save_config();
        self.set_status(Level::Info, "have fun — /guide brings the tour back");
    }

    /// WTFIS-style coffee nudge: full screen, three seconds, Enter opens it.
    fn start_coffee(&mut self) {
        self.coffee_until = Some(Instant::now() + Duration::from_secs(COFFEE_SECONDS));
        self.config.coffee_shown = Some(true);
        self.save_config();
    }

    fn dismiss_coffee(&mut self, open_link: bool) {
        self.coffee_until = None;
        if open_link {
            open_url(COFFEE_URL);
            self.set_status(Level::Ok, "thanks — opening buymeacoffee.com");
        }
    }

    fn coffee_tick(&mut self) {
        if let Some(deadline) = self.coffee_until
            && Instant::now() >= deadline
        {
            self.coffee_until = None;
        }
    }

    /// Called when a render lands: counts it and decides about the coffee.
    fn note_generation(&mut self) {
        self.runs = self.runs.saturating_add(1);
        self.config.runs = Some(self.runs);
        let due = self.config.coffee_enabled.unwrap_or(true)
            && self.runs >= COFFEE_AFTER_RUNS
            && !self.config.coffee_shown.unwrap_or(false);
        if due {
            self.start_coffee();
        } else {
            self.save_config();
        }
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
        // A quarter of a second of grace: keys that arrive with (or right after)
        // the one that opened the checklist must not make the decision.
        if self
            .finish_opened
            .is_some_and(|opened| opened.elapsed() < Duration::from_millis(250))
        {
            return;
        }
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

    /// Enter the grid: newest cell first, so the thing you just made is under
    /// the cursor.
    fn enter_grid(&mut self) {
        self.browse_mode = true;
        self.grid_on_plus = false;
        if self.selected.is_none() && !self.gallery.is_empty() {
            self.selected = Some(self.gallery.len() - 1);
        }
        self.load_selected_prompt();
    }

    fn grid_cursor(&self) -> usize {
        if self.grid_on_plus {
            self.gallery.len()
        } else {
            self.selected
                .unwrap_or(self.gallery.len().saturating_sub(1))
        }
    }

    /// Flat cursor position counted in cells (`gallery.len()` is the `+` cell).
    fn grid_set_cursor(&mut self, position: usize) {
        let cells = self.gallery.len() + 1;
        let position = position % cells.max(1);
        if position >= self.gallery.len() {
            self.grid_on_plus = true;
            self.set_status(Level::Info, "new one — type a prompt and press enter");
            return;
        }
        self.grid_on_plus = false;
        if self.selected != Some(position) {
            self.selected = Some(position);
            self.load_selected_prompt();
        }
    }

    /// Left/right move through the flat cell order; the `+` cell is part of it.
    fn grid_step(&mut self, delta: i32) {
        let cells = self.gallery.len() + 1;
        let next = grid_wrap(self.grid_cursor(), cells, delta);
        self.grid_set_cursor(next);
    }

    /// Up/down move a row at a time. Returns false when the cursor is already
    /// on the last row and the caller should hand control back to the prompt.
    fn grid_step_row(&mut self, delta: i32, columns: usize) -> bool {
        let cells = self.gallery.len() + 1;
        match grid_row_move(self.grid_cursor(), cells, columns, delta) {
            Some(target) => {
                self.grid_set_cursor(target);
                true
            }
            // Off the top: stay put. Off the bottom: back to the prompt.
            None => delta < 0,
        }
    }

    /// Open the selected picture full screen.
    fn open_viewer(&mut self) {
        let Some(item) = self.selected.and_then(|index| self.gallery.get(index)) else {
            self.set_status(Level::Err, "nothing generated yet");
            return;
        };
        let path = item.path.clone();
        let Ok(bytes) = std::fs::read(&path) else {
            self.set_status(Level::Err, "that file is gone".to_string());
            return;
        };
        let Ok(image) = image::load_from_memory(&bytes) else {
            self.set_status(Level::Err, "cannot read that image".to_string());
            return;
        };
        self.viewer = Some(Viewer {
            path,
            image,
            zoom: 1.0,
            pan_x: 0.5,
            pan_y: 0.5,
            shown: None,
            protocol: None,
        });
        self.set_status(Level::Info, "+ / - zoom · arrows pan · space closes");
    }

    /// Leave the grid and put the selected image's prompt back in the bar.
    fn leave_grid(&mut self) {
        self.browse_mode = false;
        if !self.grid_on_plus {
            self.load_selected_prompt();
        }
        self.set_status(Level::Info, "back at the prompt");
    }

    fn load_selected_prompt(&mut self) {
        if self.prompt_dirty {
            self.set_status(
                Level::Info,
                "draft kept — esc clears it, then arrows load prompts again",
            );
            return;
        }
        let Some(prompt) = self
            .selected
            .and_then(|index| self.gallery.get(index))
            .map(|item| item.prompt.clone())
        else {
            return;
        };
        self.input = prompt;
        self.cursor = self.input.chars().count();
        self.refresh_palette();
    }

    /// The default after a render: an empty prompt, with the picture still
    /// selected so the next prompt edits it.
    fn new_prompt(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.browse_mode = false;
        self.prompt_dirty = false;
        self.refresh_palette();
        self.set_status(Level::Info, "fresh prompt — type, or / for commands");
    }

    /// Re-run the selected picture's prompt, telling the model the last attempt
    /// did not land, so it comes back with a visibly different take.
    fn regenerate_selected(&mut self) {
        let Some(item) = self.selected.and_then(|index| self.gallery.get(index)) else {
            self.set_status(Level::Err, "nothing generated yet");
            return;
        };
        let prompt = nudge_prompt(&item.prompt);
        self.input = prompt;
        self.cursor = self.input.chars().count();
        self.browse_mode = false;
        self.prompt_dirty = true;
        self.submit();
    }

    /// Ask for the same picture with the background gone, so it comes back as a
    /// transparent cut-out.
    fn remove_background(&mut self) {
        let Some(item) = self.selected.and_then(|index| self.gallery.get(index)) else {
            self.set_status(Level::Err, "nothing generated yet");
            return;
        };
        let prompt = cutout_prompt(&item.prompt);
        self.input = prompt;
        self.cursor = self.input.chars().count();
        self.browse_mode = false;
        self.prompt_dirty = true;
        self.submit();
    }

    /// Put the selected picture's prompt on the system clipboard (OSC 52), so it
    /// can be pasted anywhere — terminals that do not support it simply ignore
    /// the sequence.
    fn copy_selected_prompt(&mut self) {
        let Some(prompt) = self
            .selected
            .and_then(|index| self.gallery.get(index))
            .map(|item| item.prompt.clone())
        else {
            self.set_status(Level::Err, "nothing generated yet");
            return;
        };
        if prompt.trim().is_empty() {
            self.set_status(Level::Info, "that image has no prompt recorded");
            return;
        }
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "{}", clipboard_escape(&prompt));
        let _ = stdout.flush();
        self.set_status(Level::Ok, "prompt copied to the clipboard");
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
        // Remember it for the terminal title before the worker owns it.
        self.last_prompt = prompt.clone();
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
        self.prompt_dirty = false;
        self.browse_mode = false;
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
                        needs_load: false,
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
                    // The default after a render: the + square, empty prompt.
                    self.selected = Some(self.gallery.len() - 1);
                    self.grid_on_plus = true;
                    self.set_status(
                        Level::Ok,
                        format!(
                            "generated {} ({:.1}s, {}) · staged, enter keeps it",
                            display_name(&done.path),
                            done.elapsed.as_secs_f64(),
                            human_bytes(done.bytes)
                        ),
                    );
                    self.snake.restart();
                    if self.snake.award() {
                        self.config.snake_high = Some(self.snake.high_score);
                        self.save_config();
                    }
                    self.note_generation();
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

    /// The prompt as styled spans: the character under the cursor (or a space at
    /// the end) is drawn in reverse video so the caret is unmistakable.
    fn input_spans(&self) -> Vec<Span<'static>> {
        input_spans_for(&self.input, self.cursor, self.accent)
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

/// What a plain letter means while the user is walking the gallery. Split out
/// from the key handler so the rules are testable: shortcuts only fire in
/// browse mode, only before the draft is edited, and never with no selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowseAction {
    Remove,
    Save,
    History,
    Regenerate,
    Cutout,
    CopyPrompt,
    NewPrompt,
}

fn browse_shortcut(
    key: KeyCode,
    browse_mode: bool,
    prompt_dirty: bool,
    has_selection: bool,
    input_empty: bool,
) -> Option<BrowseAction> {
    if !browse_mode || !has_selection {
        return None;
    }
    match key {
        // Deleting a file is not an edit, so it waits for an empty prompt; the
        // others are "act on this picture" verbs and only need an untouched one.
        KeyCode::Backspace | KeyCode::Delete if input_empty => Some(BrowseAction::Remove),
        KeyCode::Char('s') if !prompt_dirty => Some(BrowseAction::Save),
        KeyCode::Char('h') => Some(BrowseAction::History),
        KeyCode::Char('r') if !prompt_dirty => Some(BrowseAction::Regenerate),
        KeyCode::Char('b') if !prompt_dirty => Some(BrowseAction::Cutout),
        KeyCode::Char('y') if !prompt_dirty => Some(BrowseAction::CopyPrompt),
        KeyCode::Char('+') if input_empty || !prompt_dirty => Some(BrowseAction::NewPrompt),
        _ => None,
    }
}

/// OSC 52: put text on the system clipboard. Terminals that do not implement it
/// ignore the sequence; nothing is lost either way.
fn clipboard_escape(text: &str) -> String {
    use base64::Engine as _;
    format!(
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
    )
}

/// Split the prompt into `before`, the cursor cell (reverse video), and `after`.
fn input_spans_for(input: &str, cursor: usize, accent: Color) -> Vec<Span<'static>> {
    let chars: Vec<char> = input.chars().collect();
    let cursor = cursor.min(chars.len());
    let cursor_style = Style::default()
        .fg(Color::Black)
        .bg(accent)
        .add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    if cursor > 0 {
        spans.push(Span::raw(chars[..cursor].iter().collect::<String>()));
    }
    match chars.get(cursor) {
        Some(ch) => spans.push(Span::styled(ch.to_string(), cursor_style)),
        None => spans.push(Span::styled(" ", cursor_style)),
    }
    let after_start = cursor.saturating_add(1).min(chars.len());
    if after_start < chars.len() {
        spans.push(Span::raw(chars[after_start..].iter().collect::<String>()));
    }
    spans
}

/// Hand a URL to whatever the desktop uses for links.
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let outcome = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let outcome = Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(url)
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let outcome = Command::new("xdg-open").arg(url).spawn();
    let _ = outcome;
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let inner_width = area.width.saturating_sub(4).max(8);
    let wanted = wrapped_lines(&app.input, inner_width).clamp(1, MAX_INPUT_ROWS as usize) as u16;
    let refs_height = if app.refs.is_empty() { 0 } else { 5 };
    // The palette floats above the prompt bar instead of taking layout space:
    // opening it must not resize the grid, which would re-encode every cell.
    let palette_height = 0;
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
    if !app.palette.is_empty() {
        draw_palette_overlay(frame, app, chunks[4]);
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
    if app.history.is_some() {
        draw_history(frame, app, area);
    }
    if app.viewer.is_some() {
        draw_viewer(frame, app, area);
    }
    if app.intro.is_some() {
        draw_intro(frame, app, area);
    }
    if app.coffee_until.is_some() {
        draw_coffee(frame, app, area);
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
    if !running && let Some(item) = app.selected.and_then(|index| app.gallery.get_mut(index)) {
        if item.needs_load {
            item.needs_load = false;
            item.image = std::fs::read(&item.path)
                .ok()
                .and_then(|bytes| image::load_from_memory(&bytes).ok())
                .map(|image| image.thumbnail(2048, 2048));
        }
        if item.protocol.is_none()
            && let Some(image) = item.image.take()
        {
            item.protocol = Some(app.picker.new_resize_protocol(image));
        }
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
        && item.needs_load
    {
        item.needs_load = false;
        item.image = std::fs::read(&item.path)
            .ok()
            .and_then(|bytes| image::load_from_memory(&bytes).ok())
            .map(|image| image.thumbnail(512, 512));
    }

    if !app.gallery.is_empty() {
        draw_grid(frame, app, inner);
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

/// The gallery as a grid of squares: one cell per generation, plus the `+` cell
/// that means "new prompt". Cell size follows how many cells there are, so two
/// images fill the panel and fifteen are still all visible.
fn draw_grid(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.hit_grid.clear();
    app.snake_area = Rect::default();
    app.snake.deactivate();

    let cells = app.gallery.len() + 1;
    let (columns, rows, cell_w, cell_h) = grid_shape(cells, area);
    app.grid_columns = columns;
    let cursor = app.grid_cursor();
    let focused = app.browse_mode;

    for index in 0..cells {
        if index / columns >= rows {
            break;
        }
        let cell = grid_cell(index, columns, cell_w, cell_h, area);
        if cell.width < 4 || cell.height < 2 {
            continue;
        }
        // Decode the thumbnail the first time this cell is drawn.
        if index < app.gallery.len() {
            let item = &mut app.gallery[index];
            if item.needs_load {
                item.needs_load = false;
                item.image = std::fs::read(&item.path)
                    .ok()
                    .and_then(|bytes| image::load_from_memory(&bytes).ok())
                    .map(|image| image.thumbnail(512, 512));
            }
            if item.protocol.is_none()
                && let Some(image) = item.image.take()
            {
                item.protocol = Some(app.picker.new_resize_protocol(image));
            }
        }

        let selected = focused && index == cursor;
        let border = if selected {
            app.accent()
        } else {
            Color::DarkGray
        };
        let label = if index < app.gallery.len() {
            let item = &app.gallery[index];
            let marker = if item.saved { "✓" } else { "" };
            format!(
                " {} {}{marker} ",
                index + 1,
                truncate_chars(
                    &display_name(&item.path),
                    cell.width.saturating_sub(6) as usize
                )
            )
        } else {
            " + ".to_string()
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(Span::styled(
                label,
                Style::default().fg(border).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(cell);
        frame.render_widget(block, cell);

        if index < app.gallery.len() {
            if let Some(protocol) = app.gallery[index].protocol.as_mut() {
                frame.render_stateful_widget(
                    StatefulImage::new().resize(Resize::Fit(None)),
                    inner,
                    protocol,
                );
            }
        } else {
            let art = plus_lines(inner.width, inner.height);
            let style = Style::default()
                .fg(if selected {
                    app.accent()
                } else {
                    Color::DarkGray
                })
                .add_modifier(Modifier::BOLD);
            frame.render_widget(
                Paragraph::new(
                    art.into_iter()
                        .map(|line| Line::from(Span::styled(line, style)))
                        .collect::<Vec<_>>(),
                )
                .alignment(Alignment::Center),
                inner,
            );
        }
        app.hit_grid.push((cell, index));
    }
}

/// The history panel: kept pictures with their prompts. Enter pulls one back
/// into the grid so it can be iterated on again.
fn draw_history(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    app.hit_history.clear();
    if app.history.is_none() {
        return;
    }
    let count = app.history.as_ref().map(|h| h.rows.len()).unwrap_or(0);
    let selection = app.history.as_ref().map(|h| h.selection).unwrap_or(0);
    let width = (area.width.saturating_sub(8)).min(96);
    let row_height: u16 = 4;
    let height = (area.height.saturating_sub(4)).min(rows_to_fit(count, row_height));
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
            format!(" {ICON_HISTORY} history · {count} kept "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " ↑↓ move · enter brings it back into the grid · esc closes ",
            Style::default().fg(Color::DarkGray),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    // Decode whatever thumbnails are missing (own protocols: two render sites
    // sharing one stateful protocol would fight over its cached size).
    let missing: Vec<(usize, image::DynamicImage)> = app
        .history
        .as_ref()
        .map(|history| {
            history
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.thumb.is_none())
                .filter_map(|(index, row)| {
                    let bytes = std::fs::read(&row.path).ok()?;
                    let image = image::load_from_memory(&bytes).ok()?;
                    Some((index, image.thumbnail(256, 256)))
                })
                .collect()
        })
        .unwrap_or_default();
    for (index, image) in missing {
        let protocol = app.picker.new_resize_protocol(image);
        if let Some(history) = app.history.as_mut()
            && let Some(row) = history.rows.get_mut(index)
        {
            row.thumb = Some(protocol);
        }
    }

    let visible = usize::from(inner.height / row_height).max(1);
    let start = if selection >= visible {
        selection + 1 - visible
    } else {
        0
    };
    for index in start..(start + visible).min(count) {
        let y = inner.y + ((index - start) as u16) * row_height;
        if y + row_height > inner.y + inner.height {
            break;
        }
        let row_area = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: row_height,
        };
        app.hit_history.push((row_area, index));
        let selected = index == selection;
        let Some(history) = app.history.as_mut() else {
            return;
        };
        let Some(row) = history.rows.get_mut(index) else {
            continue;
        };
        let thumb_area = Rect {
            x: inner.x + 1,
            y,
            width: 10,
            height: row_height,
        };
        match row.thumb.as_mut() {
            Some(protocol) => frame.render_stateful_widget(
                StatefulImage::new().resize(Resize::Fit(None)),
                thumb_area,
                protocol,
            ),
            None => frame.render_widget(
                Paragraph::new(Span::styled(
                    ICON_IMAGE,
                    Style::default().fg(Color::DarkGray),
                )),
                thumb_area,
            ),
        }
        let text_area = Rect {
            x: thumb_area.x + thumb_area.width + 1,
            y,
            width: inner.width.saturating_sub(thumb_area.width + 3),
            height: row_height,
        };
        let marker = if selected { "▶ " } else { "  " };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("{marker}{}", display_name(&row.path)),
                    if selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    },
                )),
                Line::from(Span::styled(
                    truncate_chars(&row.prompt.replace('\n', " "), text_area.width as usize),
                    Style::default().fg(Color::DarkGray),
                )),
            ]),
            text_area,
        );
    }
}

/// Rows needed to show `count` rows of `row_height`, plus borders.
fn rows_to_fit(count: usize, row_height: u16) -> u16 {
    (count as u16).saturating_mul(row_height).saturating_add(2)
}

/// Full-screen look at one image: `+`/`-` zoom, arrows pan, space or esc back.
fn draw_viewer(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    if app.viewer.is_none() {
        return;
    }
    let accent = app.accent();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        area,
    );

    let key = {
        let viewer = app.viewer.as_ref().expect("checked above");
        (
            viewer.zoom,
            (viewer.pan_x * 1000.0) as i32,
            (viewer.pan_y * 1000.0) as i32,
            area.width,
            area.height,
        )
    };
    let stale = app
        .viewer
        .as_ref()
        .is_some_and(|viewer| viewer.shown != Some(key) || viewer.protocol.is_none());
    if stale {
        let cropped = {
            let viewer = app.viewer.as_ref().expect("checked above");
            viewer_crop(&viewer.image, viewer.zoom, viewer.pan_x, viewer.pan_y, area)
        };
        let protocol = app.picker.new_resize_protocol(cropped);
        if let Some(viewer) = app.viewer.as_mut() {
            viewer.protocol = Some(protocol);
            viewer.shown = Some(key);
        }
    }

    let (name, zoom) = {
        let viewer = app.viewer.as_ref().expect("checked above");
        (display_name(&viewer.path), viewer.zoom)
    };
    let controls = if zoom > 1.0 {
        "arrows pan · - zoom out"
    } else {
        "← → image · + zoom in"
    };
    let title = format!(
        " {} · {ICON_ZOOM} {:.0}% · {controls} · space closes ",
        truncate_chars(&name, 40),
        zoom * 100.0
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(
            title,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(viewer) = app.viewer.as_mut()
        && let Some(protocol) = viewer.protocol.as_mut()
    {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Fit(None)),
            inner,
            protocol,
        );
    }
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

/// Full-screen viewer state. `zoom` is a multiplier over "fit the screen", and
/// the pan is in fractions of the visible area so it survives a resize.
struct Viewer {
    path: PathBuf,
    image: image::DynamicImage,
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
    /// What the currently cached protocol was built from.
    shown: Option<(f32, i32, i32, u16, u16)>,
    protocol: Option<StatefulProtocol>,
}

/// Crop the source to what the viewer should show: `zoom` in, panned by
/// fractions of the visible area, clamped so the view never leaves the image.
fn viewer_crop(
    image: &image::DynamicImage,
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
    area: Rect,
) -> image::DynamicImage {
    let (width, height) = (image.width() as f32, image.height() as f32);
    // Fit the image into the area first, so zoom 1.0 means "whole picture".
    let area_aspect = (area.width.max(1) as f32) / (area.height.max(1) as f32 / 2.0);
    let image_aspect = width / height.max(1.0);
    let base = if image_aspect > area_aspect {
        (width, width / area_aspect)
    } else {
        (height * area_aspect, height)
    };
    let visible = (base.0 / zoom.max(1.0)).min(width).max(1.0);
    let visible_h = (base.1 / zoom.max(1.0)).min(height).max(1.0);
    let max_x = (width - visible).max(0.0);
    let max_y = (height - visible_h).max(0.0);
    let x = (max_x * pan_x).clamp(0.0, max_x);
    let y = (max_y * pan_y).clamp(0.0, max_y);
    image.crop_imm(x as u32, y as u32, visible as u32, visible_h as u32)
}

/// A big fat `+` drawn as text, sized to the cell so it reads as the "new one"
/// square from across the room.
fn plus_lines(width: u16, height: u16) -> Vec<String> {
    let width = usize::from(width).max(3);
    let rows = (usize::from(height).saturating_sub(1) | 1).clamp(3, 11);
    let bar = (rows / 5).max(1); // thickness of the strokes
    let middle = rows / 2;
    let canvas = ((width * 3) / 5).clamp(3, width); // horizontal arm length
    let pad_left = (canvas.saturating_sub(bar)) / 2;
    let pad_right = canvas.saturating_sub(bar).saturating_sub(pad_left);
    let stem = format!(
        "{}{}{}",
        " ".repeat(pad_left),
        "+".repeat(bar),
        " ".repeat(pad_right)
    );
    (0..rows)
        .map(|row| {
            if row <= middle + bar && row + bar > middle {
                "+".repeat(canvas) // horizontal arm
            } else {
                stem.clone()
            }
        })
        .collect()
}

/// Flat grid movement: `cells` includes the `+` cell, and moving past either end
/// wraps around, so left/right never dead-ends.
fn grid_wrap(current: usize, cells: usize, delta: i32) -> usize {
    if cells == 0 {
        return 0;
    }
    ((current as i32 + delta).rem_euclid(cells as i32)) as usize
}

/// Row movement. `None` means "already off the grid in that direction", which
/// is how the cursor hands control back to the prompt.
fn grid_row_move(current: usize, cells: usize, columns: usize, delta: i32) -> Option<usize> {
    let columns = columns.max(1);
    let rows = cells.div_ceil(columns);
    let row = current / columns;
    let next_row = row as i32 + delta;
    if next_row < 0 || next_row >= rows as i32 {
        return None;
    }
    let target = next_row as usize * columns + current % columns;
    (target < cells).then_some(target)
}

/// How the gallery is laid out: `count` cells (images plus the `+` cell) as a
/// near-square grid inside `area`. Fewer images means fatter cells; more images
/// means smaller ones, which is the whole point of a grid.
fn grid_shape(count: usize, area: Rect) -> (usize, usize, u16, u16) {
    if count == 0 || area.width < 6 || area.height < 4 {
        return (1, 1, area.width, area.height);
    }
    // Terminal cells are about twice as tall as they are wide, so a visually
    // square cell wants roughly two columns per row. Try every column count and
    // keep the arrangement whose cells come out largest.
    let mut best = (1usize, count, 0u16);
    for columns in 1..=count {
        let rows = count.div_ceil(columns);
        let cell_w = area.width / columns as u16;
        let cell_h = area.height / rows as u16;
        if cell_w < 6 || cell_h < 3 {
            continue;
        }
        // Score by the smaller of the two dimensions, weighted so a cell that
        // is twice as wide as tall wins (that is a square of pixels), minus a
        // penalty for cells the grid leaves empty — four images should read as
        // 2x2, not 3 plus a lonely fourth.
        let score = cell_h
            .saturating_mul(2)
            .min(cell_w)
            .saturating_sub((columns * rows - count) as u16);
        if score > best.2 {
            best = (columns, rows, score);
        }
    }
    let (columns, rows, _) = best;
    (
        columns,
        rows,
        area.width / columns as u16,
        area.height / rows as u16,
    )
}

/// The rectangle of cell `index` in a grid of `columns` inside `area`.
fn grid_cell(index: usize, columns: usize, cell_w: u16, cell_h: u16, area: Rect) -> Rect {
    let column = index % columns.max(1);
    let row = index / columns.max(1);
    Rect {
        x: area.x.saturating_add(column as u16 * cell_w),
        y: area.y.saturating_add(row as u16 * cell_h),
        width: cell_w.saturating_sub(1),
        height: cell_h.saturating_sub(1),
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
            " snake · {FIRE} {} · {} · arrows ",
            app.snake.high_score, app.snake.score
        )
    } else {
        format!(" snake · {FIRE} {} · paused ", app.snake.high_score)
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
                Some(_) => ("● ", body_style),
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

/// The command palette, floating just above the prompt bar. Drawn last so it
/// covers whatever is under it, and it never changes the layout underneath.
fn draw_palette_overlay(frame: &mut Frame<'_>, app: &mut App, prompt: Rect) {
    let rows = app.palette.len().min(8) as u16;
    let height = (rows + 2).min(prompt.y.saturating_sub(1));
    if height < 3 {
        return;
    }
    let width = prompt.width.min(80);
    let area = Rect {
        x: prompt.x + prompt.width.saturating_sub(width) / 2,
        y: prompt.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    draw_palette(frame, app, area);
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
    let mut title = if app.browse_mode {
        String::from(" prompt · browsing ")
    } else {
        String::from(" prompt ")
    };
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

    let mut spans = app.input_spans();
    if app.input.is_empty() {
        spans = vec![
            Span::styled(" ", Style::default().bg(app.accent)),
            Span::styled(
                "  Fucking type something…  (/ for commands)",
                Style::default().fg(Color::DarkGray),
            ),
        ];
    }
    let body = Line::from(spans).style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(
        Paragraph::new(body)
            .block(block)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::White)),
        area,
    );
}

fn draw_controls(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    // Lay the row out left to right so nothing can overlap the next control.
    let mut x = area.x;
    let take = |x: u16, width: u16| Rect {
        x,
        y: area.y,
        width: width.min(area.width.saturating_sub(x.saturating_sub(area.x))),
        height: 1,
    };
    app.hit_button = take(x, 15);
    x = x.saturating_add(app.hit_button.width).saturating_add(1);
    // The finish button sits next to GENERATE, capitalised, with its tick.
    app.hit_finish_button = take(x, 11);
    x = x
        .saturating_add(app.hit_finish_button.width)
        .saturating_add(1);
    app.hit_new_button = take(x, 5);
    x = x.saturating_add(app.hit_new_button.width).saturating_add(1);
    app.hit_quality = take(x, 18);
    let status_x = app
        .hit_quality
        .x
        .saturating_add(app.hit_quality.width)
        .saturating_add(1);

    let running = matches!(app.job, Job::Running { .. });
    let label = if running {
        " GENERATING… ".to_string()
    } else if app.focus == Focus::Button || app.hover == Some(Hit::Button) {
        format!("[{ICON_BOLT} GENERATE]")
    } else {
        format!(" {ICON_BOLT} GENERATE ")
    };
    let hint_text = if running {
        if app.snake.focused {
            "esc pauses the game · ctrl-c quit ".to_string()
        } else {
            "esc cancels · ctrl-c finish ".to_string()
        }
    } else if app.browse_mode {
        format!(
            "{ICON_LEFT}{ICON_RIGHT} cells · space full screen · {ICON_CHECK} enter · {ICON_REFRESH} r · {ICON_CROP} b · {ICON_COPY} y · {ICON_TRASH} del "
        )
    } else if app.selected.is_some() {
        format!("{ICON_IMAGE}↑↓ · {ICON_CHECK} enter keeps · ctrl-c finish ")
    } else {
        format!("{ICON_IMAGE}↑↓ grid · ctrl-c finish ")
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
            format!(" {ICON_SLIDERS} {} ", app.quality),
            quality_style,
        ))),
        app.hit_quality,
    );

    let new_style = if app.hover == Some(Hit::NewPrompt) {
        Style::default().fg(Color::Black).bg(app.accent())
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" [{ICON_PLUS}] "),
            new_style,
        ))),
        app.hit_new_button,
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
        Paragraph::new(Line::from(Span::styled(
            format!(" [{ICON_CHECK} FINISH] "),
            finish_style,
        ))),
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
        format!(
            "coffee popup {}",
            if app.config.coffee_enabled.unwrap_or(true) {
                "on"
            } else {
                "off"
            }
        ),
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

/// OSC 0: set the icon name and window/tab title. Terminals that do not know
/// it ignore the sequence, and the title stack push/pop around it restores the
/// shell's own title on the way out.
fn title_escape(title: &str) -> String {
    format!("\x1b]0;{title}\x07")
}

/// What the terminal tab shows. Kept separate so the rules are testable:
/// the checklist question wins, then the spinner + the prompt being rendered,
/// then whatever is typed, then the newest generation, then the tool name.
fn window_title(
    spinner: &str,
    running: bool,
    rendered_prompt: &str,
    draft: &str,
    finish_rows: Option<usize>,
) -> String {
    let raw = if let Some(rows) = finish_rows {
        format!("keep {rows} image(s)?")
    } else if running {
        format!("{spinner} {rendered_prompt}")
    } else if !draft.trim().is_empty() {
        draft.trim().to_string()
    } else if !rendered_prompt.trim().is_empty() {
        rendered_prompt.trim().to_string()
    } else {
        "fgen".to_string()
    };
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&one_line, 48)
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

/// "Another take" prompt: same subject, visibly different treatment.
fn nudge_prompt(prompt: &str) -> String {
    format!(
        "{prompt}

The previous attempt at this did not land. Keep the same subject and intent, but take a clearly different approach — different style, lighting and composition — so this is a genuinely new option rather than a variation."
    )
}

/// "Cut it out" prompt: same subject, transparent background.
fn cutout_prompt(prompt: &str) -> String {
    format!(
        "{prompt}

Keep the subject exactly as it is and remove the background completely: a clean cut-out on a fully transparent background (transparent PNG, no backdrop, no shadow on a surface)."
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
    if app.finish.is_none() {
        return;
    }

    let width = 88.min(area.width);
    // Every row shows its picture, so rows are thumbnail-sized, not one line.
    let row_height: u16 = 6;
    let rows = app
        .finish
        .as_ref()
        .map(|finish| finish.rows.len())
        .unwrap_or(0) as u16;
    let height = (rows * row_height + 8).min(area.height);
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
    frame.render_widget(Paragraph::new(lines), inner);

    // Each row: tick box, a real thumbnail of the picture, then its details.
    let thumb_w: u16 = 14;
    // Decode any missing thumbnails first, so the picker borrow and the
    // checklist borrow never overlap.
    let missing: Vec<(usize, image::DynamicImage)> = app
        .finish
        .as_ref()
        .map(|finish| {
            finish
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.thumb.is_none())
                .filter_map(|(index, row)| {
                    let bytes = std::fs::read(&row.path).ok()?;
                    let image = image::load_from_memory(&bytes).ok()?;
                    Some((index, image.thumbnail(256, 256)))
                })
                .collect()
        })
        .unwrap_or_default();
    for (index, image) in missing {
        let protocol = app.picker.new_resize_protocol(image);
        if let Some(finish) = app.finish.as_mut()
            && let Some(row) = finish.rows.get_mut(index)
        {
            row.thumb = Some(protocol);
        }
    }

    let selection = app
        .finish
        .as_ref()
        .map(|finish| finish.selection)
        .unwrap_or(0);
    let row_count = app
        .finish
        .as_ref()
        .map(|finish| finish.rows.len())
        .unwrap_or(0);
    for (index, row_area) in finish_row_rects(inner, list_top, row_height, row_count, selection) {
        let row_y = row_area.y;
        app.hit_finish_rows.push((row_area, index));
        let selected = index == selection;
        frame.render_widget(
            Paragraph::new("").style(if selected {
                Style::default().bg(accent)
            } else {
                Style::default()
            }),
            row_area,
        );

        // One borrow for the whole row: everything below uses locals.
        let Some(finish) = app.finish.as_mut() else {
            return;
        };
        let Some(row) = finish.rows.get_mut(index) else {
            continue;
        };

        let tick = if row.keep { "[x]" } else { "[ ]" };
        let tick_area = Rect {
            x: inner.x + 1,
            y: row_y,
            width: 4,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                tick.to_string(),
                if selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(accent)
                        .add_modifier(Modifier::BOLD)
                } else if row.keep {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            )),
            tick_area,
        );

        // The picture itself, not a placeholder box.
        let image_area = Rect {
            x: inner.x + 6,
            y: row_y,
            width: thumb_w.min(inner.width.saturating_sub(8)),
            height: row_height,
        };
        match row.thumb.as_mut() {
            Some(protocol) => frame.render_stateful_widget(
                StatefulImage::new().resize(Resize::Fit(None)),
                image_area,
                protocol,
            ),
            None => frame.render_widget(
                Paragraph::new(Span::styled("▣", Style::default().fg(Color::DarkGray)))
                    .alignment(Alignment::Center),
                image_area,
            ),
        }

        let text_area = Rect {
            x: image_area.x + image_area.width + 1,
            y: row_y,
            width: inner
                .width
                .saturating_sub(image_area.x - inner.x + image_area.width + 1),
            height: row_height,
        };
        let details = format!("{} · {}", display_name(&row.path), human_bytes(row.bytes));
        let prompt = row.prompt.replace('\n', " ");
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    truncate_chars(&details, text_area.width as usize),
                    if selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    },
                )),
                Line::from(Span::styled(
                    truncate_chars(&prompt, text_area.width as usize),
                    if selected {
                        Style::default().fg(Color::Black).bg(accent)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                )),
            ])
            .wrap(Wrap { trim: true }),
            text_area,
        );
    }

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

/// The first-run tour, in the spirit of WTFIS's intro panel.
fn draw_intro(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(page) = app.intro else {
        return;
    };
    let (title, body) = &INTRO_PAGES[page.min(INTRO_PAGES.len() - 1)];
    // Painted over everything: the tour is a full-screen takeover.
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(app.accent)),
        area,
    );
    let width = 74.min(area.width);
    let height = (body.len() as u16 + BANNER.len() as u16 + 7).min(area.height);
    let panel = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    app.hit_intro = panel;
    frame.render_widget(Clear, panel);
    let accent = app.accent();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title(Span::styled(
            format!(" fgen · {title} "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            format!(
                " {}/{} · ← → pages · enter next · esc skip ",
                page + 1,
                INTRO_PAGES.len()
            ),
            Style::default().fg(Color::DarkGray),
        ));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);
    let mut lines: Vec<Line> = Vec::new();
    for line in BANNER {
        lines.push(Line::from(Span::styled(
            line,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    for line in body {
        lines.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(Color::White),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        inner,
    );
}

/// Which checklist rows are visible and where each one sits. Rows are tall
/// enough for a thumbnail, and the list scrolls to keep the cursor in view.
fn finish_row_rects(
    inner: Rect,
    list_top: u16,
    row_height: u16,
    rows: usize,
    selection: usize,
) -> Vec<(usize, Rect)> {
    if rows == 0 || row_height == 0 {
        return Vec::new();
    }
    let visible = usize::from(inner.height.saturating_sub(2) / row_height).max(1);
    let start = if selection >= visible {
        selection + 1 - visible
    } else {
        0
    };
    let mut rects = Vec::new();
    for index in start..(start + visible).min(rows) {
        let y = list_top + ((index - start) as u16) * row_height;
        if y + row_height > inner.y + inner.height {
            break;
        }
        rects.push((
            index,
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: row_height,
            },
        ));
    }
    rects
}

/// What the coffee popup says, WTFIS-style: loud, short, three seconds./// What the coffee popup says, WTFIS-style: loud, short, three seconds.
fn coffee_lines(remaining: Duration) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "Fgen keeping you fed?☕️",
            Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "[ OKAY ]",
            Style::default()
                .fg(Color::Yellow)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Enter to open  •  Esc to skip",
            Style::default().fg(Color::Black),
        )),
        Line::from(Span::styled(
            format!("auto-closing in {}s", remaining.as_secs().saturating_add(1)),
            Style::default().fg(Color::Black),
        )),
    ]
}

/// The WTFIS coffee popup: loud, brief, and only ever once.
fn draw_coffee(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let Some(deadline) = app.coffee_until else {
        return;
    };
    app.hit_coffee = area;
    let remaining = deadline.saturating_duration_since(Instant::now());
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Yellow).fg(Color::Black)),
        area,
    );
    let width = area.width.saturating_sub(4).min(64);
    let height = area.height.saturating_sub(4).min(9);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Black))
        .style(Style::default().bg(Color::Yellow).fg(Color::Black))
        .title(Span::styled(
            COFFEE_TITLE,
            Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(coffee_lines(remaining))
            .alignment(Alignment::Center)
            .style(Style::default().bg(Color::Yellow).fg(Color::Black)),
        inner,
    );
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
            " ↑↓ scroll · pgup/pgdn · esc or q closes ",
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
    // Scrolling: clip the list to the panel and say where we are.
    let visible = usize::from(inner.height);
    let offset = app.help_scroll.min(lines.len().saturating_sub(1));
    let end = (offset + visible).min(lines.len());
    frame.render_widget(Paragraph::new(lines[offset..end].to_vec()), inner);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}/{} ", end.min(lines.len()), lines.len()),
            Style::default().fg(Color::DarkGray),
        ))
        .alignment(Alignment::Right),
        Rect {
            x: inner.x,
            y: panel.y + panel.height.saturating_sub(1),
            width: inner.width,
            height: 0,
        },
    );
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

/// Where the coffee nudge points, exactly like the WTFIS one.
const COFFEE_URL: &str = "https://buymeacoffee.com/professorvolodymyr";
/// Generations before the one-time coffee popup appears.
const COFFEE_AFTER_RUNS: u32 = 10;
const COFFEE_SECONDS: u64 = 3;
const COFFEE_TITLE: &str = " BUY ME A COFFEE ";

/// Block-letter banner for the tour, drawn as text so it works in any terminal.
const BANNER: [&str; 5] = [
    "  ███████╗ ██████╗ ███████╗███╗   ██╗",
    "  ██╔════╝██╔════╝ ██╔════╝████╗  ██║",
    "  █████╗  ██║  ███╗█████╗  ██╔██╗ ██║",
    "  ██╔══╝  ██║   ██║██╔══╝  ██║╚██╗██║",
    "  ██║     ╚██████╔╝███████╗██║ ╚████║",
];

/// The first-run tour. Four short pages, because fgen does more than it looks.
const INTRO_PAGES: [(&str, [&str; 5]); 4] = [
    (
        "fgen",
        [
            "Images from your own ChatGPT subscription.",
            "No API key, no per-image invoice.",
            "",
            "Type a prompt, press enter, and the picture lands in the gallery.",
            "Nothing is written to your folders until you say so.",
        ],
    ),
    (
        "the gallery",
        [
            "Up and down walk the images you made this session.",
            "The prompt bar shows that image's prompt again — edit it and press",
            "enter to make a new version of it, or use the shortcuts:",
            "",
            "enter keep · del remove · r another take · b cut the background · y copy the prompt",
        ],
    ),
    (
        "keeping pictures",
        [
            "Renders are staged in a session cache, not in your folders.",
            "enter keeps the one you are looking at; ctrl-c (or [finish]) asks",
            "about the rest: space ticks, enter keeps the ticked ones, n keeps",
            "nothing. A killed window keeps its staged renders for next time.",
            "",
        ],
    ),
    (
        "while it renders",
        [
            "A small snake board shows up in the middle — arrows steer it",
            "immediately, edges wrap, esc pauses.",
            "",
            "The terminal tab follows along: prompt, spinner while rendering,",
            "and the checklist question on the way out. /guide shows this again.",
        ],
    ),
];

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
    // Push the terminal title so we can put the user's own back on the way out
    // (xterm title stack; terminals without it just ignore both sequences).
    let _ = std::io::stdout().write_all(b"\x1b[22;2t");
    let _ = std::io::stdout().flush();
    // Click-only mouse tracking (X10 + SGR). Any-motion mode is deliberately
    // left off: with it enabled macOS terminals route a Finder drag to the app
    // as mouse events and the dropped file path never arrives.
    let _ = std::io::stdout().write_all(b"\x1b[?1000h\x1b[?1006h");
    let _ = std::io::stdout().flush();
    let result = app.run(&mut terminal);
    let _ = std::io::stdout().write_all(b"\x1b[?1006l\x1b[?1000l");
    let _ = std::io::stdout().write_all(b"\x1b[23;2t");
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result?;
    report_session(&mut app);
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

fn report_session(app: &mut App) {
    // Save the best run on the way out, so a high score can never be lost to a
    // quit that happened between generations.
    if app.snake.award() {
        app.config.snake_high = Some(app.snake.high_score);
        app.save_config();
    }
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
    fn terminal_title_follows_the_work() {
        let spinner = "⠋";
        // Idle with nothing to say: the tool name.
        assert_eq!(window_title(spinner, false, "", "", None), "fgen");
        // The newest generation names the tab.
        assert_eq!(
            window_title(spinner, false, "a girl holding a cup", "", None),
            "a girl holding a cup"
        );
        // Typing takes over before you hit enter.
        assert_eq!(
            window_title(spinner, false, "a girl holding a cup", "make it blue", None),
            "make it blue"
        );
        // While rendering: spinner + prompt, and newlines collapse.
        assert_eq!(
            window_title(spinner, true, "poster\nA   B", "", None),
            "⠋ poster A B"
        );
        // The checklist question outranks everything.
        assert_eq!(
            window_title(spinner, true, "poster", "typing", Some(3)),
            "keep 3 image(s)?"
        );
        // And it never runs away with the tab.
        let long = window_title(spinner, false, &"x".repeat(200), "", None);
        assert!(long.chars().count() <= 49, "{long}");
        // The escape that actually reaches the terminal is OSC 0 + BEL.
        assert_eq!(title_escape("fgen"), "\u{1b}]0;fgen\u{7}");
    }

    #[test]
    fn the_plus_cell_draws_a_big_plus() {
        let art = plus_lines(40, 9);
        assert!(art.len() >= 3 && art.len() % 2 == 1, "{art:?}");
        // Every line is the same width, so it stays centred.
        let widths: std::collections::BTreeSet<usize> =
            art.iter().map(|line| line.chars().count()).collect();
        assert_eq!(widths.len(), 1, "ragged lines: {art:?}");
        // The middle is the arm, and it is wider than the stem rows.
        let middle = art[art.len() / 2].matches('+').count();
        assert!(middle > art[0].matches('+').count(), "{art:?}");
        // It fits the cell it was asked for.
        assert!(art.len() as u16 <= 9);
        assert!(art[0].chars().count() as u16 <= 40);
        // A cramped cell still produces something.
        assert!(!plus_lines(3, 2).is_empty());
    }

    #[test]
    fn history_and_save_are_reachable_from_the_grid() {
        use BrowseAction::*;
        assert_eq!(
            browse_shortcut(KeyCode::Char('s'), true, false, true, false),
            Some(Save),
            "s keeps the selected picture"
        );
        assert_eq!(
            browse_shortcut(KeyCode::Char('h'), true, false, true, false),
            Some(History),
            "h opens what you kept before"
        );
        assert_eq!(
            browse_shortcut(KeyCode::Char('s'), true, true, true, false),
            None,
            "still typing? s is a letter"
        );
    }

    #[test]
    fn grid_navigation_wraps_and_exits_at_the_bottom() {
        // Three images plus the + cell, three columns: one row.
        let cells = 4;
        assert_eq!(grid_wrap(0, cells, -1), 3, "left from the first cell wraps");
        assert_eq!(grid_wrap(3, cells, 1), 0, "right from the + cell wraps");
        assert_eq!(grid_wrap(1, cells, 1), 2);
        // Single row: down means "done browsing", up stays put.
        assert_eq!(grid_row_move(0, cells, 3, -1), None);
        assert_eq!(grid_row_move(2, cells, 3, 1), None);

        // Two rows of three: down moves within the grid, up comes back.
        let cells = 6;
        assert_eq!(grid_row_move(1, cells, 3, 1), Some(4));
        assert_eq!(grid_row_move(4, cells, 3, -1), Some(1));
        // Last row down and first row up are the exits.
        assert_eq!(grid_row_move(4, cells, 3, 1), None);
        assert_eq!(grid_row_move(1, cells, 3, -1), None);
        // Stops at the missing cell instead of walking off the end.
        assert_eq!(
            grid_row_move(2, 5, 3, 1),
            None,
            "only two cells on that row"
        );
        assert_eq!(grid_wrap(0, 0, 1), 0, "no cells, no panic");
    }

    #[test]
    fn viewer_crop_respects_zoom_and_pan() {
        // A gradient, so a crop's position is readable from its first pixel.
        let mut buffer = image::RgbaImage::new(400, 200);
        for x in 0..400u32 {
            for y in 0..200u32 {
                buffer.put_pixel(x, y, image::Rgba([(x / 2) as u8, (y / 2) as u8, 0, 255]));
            }
        }
        let image = image::DynamicImage::ImageRgba8(buffer);

        // Wide area: zoom crops the height, since the width already fits.
        let wide = Rect::new(0, 0, 40, 20);
        let full = viewer_crop(&image, 1.0, 0.5, 0.5, wide);
        assert!(
            full.width() >= 390 && full.height() >= 190,
            "{}x{}",
            full.width(),
            full.height()
        );
        let zoomed = viewer_crop(&image, 2.0, 0.5, 0.5, wide);
        assert!(
            zoomed.height() < full.height(),
            "zoom should show less: {} vs {}",
            zoomed.height(),
            full.height()
        );

        // Narrow area: zoom crops the width, and pan moves that window.
        let narrow = Rect::new(0, 0, 20, 40);
        let left = viewer_crop(&image, 2.0, 0.0, 0.5, narrow);
        let right = viewer_crop(&image, 2.0, 1.0, 0.5, narrow);
        assert!(
            left.width() < image.width(),
            "zoom shows a slice, not everything"
        );
        assert_eq!(
            left.width(),
            right.width(),
            "pan does not change the window size"
        );
        let first = |crop: &image::DynamicImage| crop.to_rgba8().get_pixel(0, 0)[0] as u32 * 2;
        assert_eq!(first(&left), 0, "left edge of the picture");
        assert_eq!(
            first(&right),
            image.width() - left.width(),
            "panned to the right edge"
        );
    }

    #[test]
    fn grid_cells_shrink_as_images_pile_up() {
        let area = Rect::new(0, 0, 120, 40);
        // Two images plus the + cell: three fat cells in one row.
        let (columns, rows, cell_w, cell_h) = grid_shape(3, area);
        assert_eq!((columns, rows), (3, 1));
        assert!(cell_w >= 40 && cell_h >= 39, "{cell_w}x{cell_h}");

        // Four cells: a balanced 2x2 beats a lopsided 3+1.
        let (columns, rows, _, _) = grid_shape(4, area);
        assert_eq!((columns, rows), (2, 2));

        // Many images: the grid grows rows and the cells get smaller.
        let (columns, rows, small_w, small_h) = grid_shape(17, area);
        assert!(rows >= 2, "a full gallery wraps to {} rows", rows);
        assert!(small_w < cell_w && small_h < cell_h, "{small_w}x{small_h}");
        assert!(columns * rows >= 17, "every cell has a slot");

        // Cells never overlap and stay inside the area.
        let last = grid_cell(16, columns, small_w, small_h, area);
        assert!(last.x + last.width <= area.width);
        assert!(last.y + last.height <= area.height);
        let first = grid_cell(0, columns, small_w, small_h, area);
        assert_eq!((first.x, first.y), (0, 0));
        let next = grid_cell(1, columns, small_w, small_h, area);
        assert_eq!(next.x, first.x + small_w, "cells tile without gaps");
    }

    #[test]
    fn grid_shape_survives_a_tiny_terminal() {
        let (columns, rows, _, _) = grid_shape(5, Rect::new(0, 0, 20, 6));
        assert!(columns >= 1 && rows >= 1);
        let (columns, rows, _, _) = grid_shape(0, Rect::new(0, 0, 120, 40));
        assert_eq!((columns, rows), (1, 1));
    }

    #[test]
    fn snake_high_score_only_moves_up() {
        let mut game = SnakeGame::new();
        game.resize(12, 12);
        game.score = 0;
        assert!(!game.award(), "nothing to celebrate yet");
        assert_eq!(game.high_score, 0);
        game.score = 5;
        assert!(game.award());
        assert_eq!(game.high_score, 5);
        game.score = 2;
        assert!(!game.award(), "a worse run does not lower the best");
        assert_eq!(game.high_score, 5);
    }

    #[test]
    fn a_finished_generation_restarts_the_board() {
        let mut game = SnakeGame::new();
        game.resize(12, 12);
        game.focused = true;
        game.score = 4;
        let before = game.body[0];
        game.step();
        game.restart();
        assert_eq!(game.score, 0, "score resets");
        assert_eq!(
            game.body.len(),
            (12 / 4).clamp(4, 10),
            "starter length again"
        );
        assert!(game.focused, "and it is still listening");
        let _ = before;
    }

    #[test]
    fn browse_shortcuts_only_fire_on_an_untouched_selection() {
        use BrowseAction::*;
        // Walking the gallery with a loaded, untouched prompt: the verbs work.
        assert_eq!(
            browse_shortcut(KeyCode::Char('r'), true, false, true, false),
            Some(Regenerate)
        );
        assert_eq!(
            browse_shortcut(KeyCode::Char('b'), true, false, true, false),
            Some(Cutout)
        );
        assert_eq!(
            browse_shortcut(KeyCode::Char('y'), true, false, true, false),
            Some(CopyPrompt)
        );
        // Editing the draft ends the shortcuts so letters type again.
        assert_eq!(
            browse_shortcut(KeyCode::Char('r'), true, true, true, false),
            None
        );
        assert_eq!(
            browse_shortcut(KeyCode::Char('b'), true, true, true, false),
            None
        );
        // Not browsing (or nothing selected): plain typing.
        assert_eq!(
            browse_shortcut(KeyCode::Char('r'), false, false, true, true),
            None
        );
        assert_eq!(
            browse_shortcut(KeyCode::Char('r'), true, false, false, true),
            None
        );
        // Deleting waits for an empty prompt, so it never eats an edit.
        assert_eq!(
            browse_shortcut(KeyCode::Backspace, true, false, true, true),
            Some(Remove)
        );
        assert_eq!(
            browse_shortcut(KeyCode::Backspace, true, false, true, false),
            None
        );
        assert_eq!(
            browse_shortcut(KeyCode::Delete, true, false, true, true),
            Some(Remove)
        );
        // Plain letters and unrelated keys stay typing.
        assert_eq!(
            browse_shortcut(KeyCode::Char('q'), true, false, true, true),
            None
        );
        assert_eq!(
            browse_shortcut(KeyCode::Enter, true, false, true, true),
            None
        );
    }

    #[test]
    fn clipboard_escape_is_osc52() {
        let escape = clipboard_escape("a girl holding a cup");
        assert!(escape.starts_with("\u{1b}]52;c;"), "{escape}");
        assert!(escape.ends_with('\u{7}'));
        let payload = escape
            .trim_start_matches("\u{1b}]52;c;")
            .trim_end_matches('\u{7}');
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "a girl holding a cup");
    }

    #[test]
    fn checklist_rows_fit_thumbnails_and_scroll_with_the_cursor() {
        let inner = Rect::new(2, 10, 80, 30);
        // Every row is tall enough to show a picture, and starts below the header.
        let rects = finish_row_rects(inner, 13, 6, 2, 0);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].1.y, 13);
        assert_eq!(rects[0].1.height, 6);
        assert_eq!(rects[1].1.y, 19, "rows follow each other");
        assert!(rects[0].1.x == inner.x && rects[0].1.width == inner.width);

        // Deep in a long list the window scrolls so the cursor stays visible.
        let many = finish_row_rects(inner, 13, 6, 40, 20);
        let indexes: Vec<usize> = many.iter().map(|(index, _)| *index).collect();
        assert!(
            indexes.contains(&20),
            "cursor row is on screen: {indexes:?}"
        );
        assert!(indexes.windows(2).all(|pair| pair[1] == pair[0] + 1));
        // And nothing is drawn past the bottom of the panel.
        assert!(
            many.iter()
                .all(|(_, rect)| rect.y + rect.height <= inner.y + inner.height)
        );

        // Degenerate inputs do not panic.
        assert!(finish_row_rects(inner, 13, 6, 0, 0).is_empty());
        assert!(finish_row_rects(inner, 13, 0, 5, 0).is_empty());
    }

    #[test]
    fn coffee_popup_says_what_it_does() {
        let lines = coffee_lines(Duration::from_secs(2));
        let text: String = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(COFFEE_TITLE.to_lowercase().contains("coffee"));
        assert!(text.contains("keeping you fed"), "{text}");
        assert!(text.contains("[ OKAY ]"));
        assert!(text.contains("Enter to open"));
        assert!(text.contains("Esc to skip"));
        // Countdown is rounded up so it never reads "0s" while still visible.
        assert!(text.contains("auto-closing in 3s"), "{text}");
    }

    #[test]
    fn cursor_spans_cover_every_position() {
        let accent = Color::Blue;
        // Empty prompt: just the reverse-video block (this used to panic).
        let spans = input_spans_for("", 0, accent);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, " ");

        let spans = input_spans_for("hi", 0, accent);
        assert_eq!(spans.len(), 2, "block + remainder");
        assert_eq!(spans[0].content, "h");
        assert_eq!(spans[1].content, "i");

        let spans = input_spans_for("hii", 1, accent);
        assert_eq!(spans.len(), 3, "before + block + after");
        assert_eq!(spans[0].content, "h");
        assert_eq!(spans[1].content, "i");
        assert_eq!(spans[2].content, "i");

        // Cursor on the last character: nothing follows the block.
        let spans = input_spans_for("hi", 1, accent);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content, "i");

        // Cursor past the end is clamped, never sliced out of range.
        let spans = input_spans_for("hi", 99, accent);
        assert_eq!(spans.len(), 2, "before + trailing block");
        assert_eq!(spans[1].content, " ");
    }

    #[test]
    fn gallery_nudges_keep_the_original_prompt() {
        let asked = "a girl holding a cup";
        let again = nudge_prompt(asked);
        assert!(again.starts_with(asked), "the user's words come first");
        assert!(again.contains("did not land"));
        assert!(again.contains("different approach"));

        let cut = cutout_prompt(asked);
        assert!(cut.starts_with(asked));
        assert!(cut.contains("transparent background"));
        assert!(cut.contains("cut-out"));

        // Nothing in the injections replaces the prompt the user wrote.
        assert_eq!(again.lines().next(), Some(asked));
        assert_eq!(cut.lines().next(), Some(asked));
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
                thumb: None,
            },
            FinishRow {
                path: collides.clone(),
                prompt: "taken".to_string(),
                bytes: 6,
                keep: true,
                thumb: None,
            },
            FinishRow {
                path: drop.clone(),
                prompt: "drop-me".to_string(),
                bytes: 7,
                keep: false,
                thumb: None,
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
            needs_load: false,
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
                thumb: None,
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
        // A normal panel keeps the board compact: the cap rules, not the space.
        assert_eq!(snake_board_cells(Rect::new(0, 0, 80, 30)), Some(14));
        // Narrow panel: columns become the limit ((16 - 2) / 2 = 7).
        assert_eq!(snake_board_cells(Rect::new(0, 0, 16, 30)), Some(7));
        // The board never exceeds the cap, even in a huge terminal.
        assert_eq!(snake_board_cells(Rect::new(0, 0, 400, 200)), Some(14));
        // Too small to play: no board at all (columns cap out below the minimum).
        assert_eq!(snake_board_cells(Rect::new(0, 0, 10, 30)), None);
        // Six cells still counts as playable.
        assert_eq!(snake_board_cells(Rect::new(0, 0, 20, 8)), Some(6));
    }

    #[test]
    fn snake_body_is_solid_dots() {
        // One glyph for the whole body: head in the accent colour, the rest white.
        let mut game = SnakeGame::new();
        game.resize(14, 14);
        assert!(game.body.len() >= 3);
        assert!(game.body.iter().all(|point| point.x < 14 && point.y < 14));
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
