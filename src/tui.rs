use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
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

const TICK: Duration = Duration::from_millis(80);
const MAX_INPUT_ROWS: u16 = 12;
const MAX_GALLERY: usize = 16;
const AUTH_REFRESH_TICKS: u64 = 40;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const QUALITIES: [&str; 4] = ["low", "medium", "high", "auto"];
const THEMES: [(&str, Color); 7] = [
    ("blue", Color::Blue),
    ("lightblue", Color::LightBlue),
    ("cyan", Color::Cyan),
    ("magenta", Color::Magenta),
    ("green", Color::Green),
    ("yellow", Color::Yellow),
    ("white", Color::White),
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
        description: "open the selected image in Preview",
    },
    CommandSpec {
        name: "/view",
        args: "",
        description: "reveal the selected image in Finder",
    },
    CommandSpec {
        name: "/root",
        args: "",
        description: "move session images to ~/Downloads and save there",
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
    Palette(usize),
    Reference(usize),
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
    image: Option<image::DynamicImage>,
    protocol: Option<StatefulProtocol>,
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

struct App {
    picker: Picker,
    input: String,
    cursor: usize,
    refs: Vec<Reference>,
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
    hit_input: Rect,
    hit_button: Rect,
    hit_quality: Rect,
    hit_palette: Vec<(Rect, usize)>,
    hit_refs: Vec<(Rect, usize)>,
    hit_settings: Vec<(Rect, usize)>,
    palette: Vec<usize>,
    palette_selection: usize,
    settings: Settings,
    help_open: bool,
    auth_ok: bool,
    auth_note: String,
    quit: bool,
    pending_login: bool,
}

impl App {
    fn new(args: TuiArgs) -> Result<Self> {
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
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
        let input = args.initial_prompt.unwrap_or_default();
        let cursor = input.chars().count();
        let mut app = App {
            picker,
            input,
            cursor,
            refs: Vec::new(),
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
            hover: None,
            hit_input: Rect::default(),
            hit_button: Rect::default(),
            hit_quality: Rect::default(),
            hit_palette: Vec::new(),
            hit_refs: Vec::new(),
            hit_settings: Vec::new(),
            palette: Vec::new(),
            palette_selection: 0,
            settings: Settings::default(),
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
                let _ = execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);
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
        if self.hit_button.contains(position) {
            return Some(Hit::Button);
        }
        if self.hit_quality.contains(position) {
            return Some(Hit::Quality);
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
                    Some(Hit::Input) => self.focus = Focus::Input,
                    Some(Hit::Palette(index)) => {
                        self.focus = Focus::Input;
                        self.palette_selection = index;
                        self.complete_palette();
                    }
                    Some(Hit::Reference(index)) => self.remove_reference(index),
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
        if let Some(path) = paste_reference(text) {
            self.attach_reference(path);
            return;
        }
        self.focus = Focus::Input;
        insert_text(&mut self.input, &mut self.cursor, text);
        self.refresh_palette();
    }

    fn attach_reference(&mut self, path: PathBuf) {
        if self.refs.iter().any(|item| item.path == path) {
            self.set_status(
                Level::Info,
                format!("{} is already attached", display_name(&path)),
            );
            return;
        }
        let image = std::fs::read(&path)
            .ok()
            .and_then(|bytes| image::load_from_memory(&bytes).ok());
        let decoded = image.is_some();
        self.refs.push(Reference {
            path: path.clone(),
            image,
            protocol: None,
        });
        self.set_status(
            if decoded { Level::Ok } else { Level::Info },
            format!("attached {}", display_name(&path)),
        );
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
        let palette_open = !self.palette.is_empty();
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('q'), KeyModifiers::CONTROL) => self.quit = true,
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
                if let Job::Running { cancel, .. } = &self.job {
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
                if self.is_command_line() {
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
                } else {
                    self.submit();
                }
            }
            (KeyCode::Char(' '), KeyModifiers::NONE) if self.focus == Focus::Button => {
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
        self.input.trim_start().starts_with('/')
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
        #[cfg(not(target_os = "macos"))]
        let outcome = {
            let target = if in_finder {
                path.parent().unwrap_or(&path).to_path_buf()
            } else {
                path.clone()
            };
            Command::new("xdg-open").arg(&target).spawn()
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
        let ref_paths: Vec<PathBuf> = self.refs.iter().map(|item| item.path.clone()).collect();
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
                        image: Some(done.image),
                        protocol: None,
                    });
                    if self.gallery.len() > MAX_GALLERY {
                        self.gallery.remove(0);
                    }
                    self.selected = Some(self.gallery.len() - 1);
                    self.set_status(
                        Level::Ok,
                        format!(
                            "saved {} ({:.1}s, {}) · {}/{}",
                            done.path.display(),
                            done.elapsed.as_secs_f64(),
                            human_bytes(done.bytes),
                            self.gallery.len(),
                            self.gallery.len()
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
            let _ = state::record(&path, &prompt);
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
    let item = app.selected.and_then(|index| app.gallery.get_mut(index));
    if let Some(item) = item
        && item.protocol.is_none()
        && let Some(image) = item.image.take()
    {
        item.protocol = Some(app.picker.new_resize_protocol(image));
    }
    let title = match app.selected.and_then(|index| app.gallery.get(index)) {
        Some(item) => format!(
            " output · {} · {}/{} · {} ",
            display_name(&item.path),
            app.selected.unwrap_or(0) + 1,
            app.gallery.len(),
            human_bytes(item.bytes)
        ),
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

    if let Some(item) = app.selected.and_then(|index| app.gallery.get_mut(index))
        && let Some(protocol) = item.protocol.as_mut()
    {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Fit(None)),
            inner,
            protocol,
        );
        return;
    }

    if let Job::Running { started, phase, .. } = &app.job {
        let lines = marquee_lines(
            inner.width,
            app.tick,
            phase,
            started.elapsed(),
            app.accent(),
        );
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        return;
    }

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
    let status_x = app.hit_quality.x + app.hit_quality.width + 1;

    let running = matches!(app.job, Job::Running { .. });
    let label = if running {
        " GENERATING…"
    } else if app.focus == Focus::Button || app.hover == Some(Hit::Button) {
        "[⏎ GENERATE]"
    } else {
        " ⏎ GENERATE "
    };
    let hint_text = if running {
        "esc cancel · ctrl-c quit "
    } else {
        "↑↓ gallery · shift+enter newline · esc cancel · ctrl-c quit "
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
        ("enter", "generate · run a /command"),
        ("shift+enter", "new line, the prompt box grows with it"),
        ("↑ ↓", "walk the generated images"),
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
    let _ = execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);
    let result = app.run(&mut terminal);
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
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
    fn gallery_selection_walks_and_clamps() {
        assert_eq!(next_selection(0, None, -1), None);
        assert_eq!(next_selection(3, None, -1), Some(2));
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
