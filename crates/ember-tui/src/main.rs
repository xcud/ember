//! ember TUI (MVP + instance/modpack management).
//!
//! Lists instances, launches one (streaming its log into a PTY-backed console
//! pane via `ember-term`), and manages instances: new, clone, delete, and
//! import a Modrinth `.mrpack`. This is the spine that grows into the
//! multiplexed companion shell (game console + shell + AI chat).

use std::io::Stdout;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use ember_term::PtySession;
use launcher_core::auth::Account;
use launcher_core::instance::Instance;
use launcher_core::launch::{AuthSession, Host};
use launcher_core::modpack;
use launcher_core::modrinth::Client;

/// A pending modal action awaiting text input or confirmation.
#[derive(PartialEq)]
enum Modal {
    None,
    NewName,
    CloneName,
    ImportPath,
    ConfirmDelete,
}

struct App {
    instances: Vec<Instance>,
    selected: usize,
    console: Option<PtySession>,
    status: String,
    modal: Modal,
    input: String,
}

fn default_cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"))
        .join("ember")
}

fn default_mc_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".minecraft")
}

impl App {
    fn new() -> Self {
        let mut app = App {
            instances: Vec::new(),
            selected: 0,
            console: None,
            status: String::new(),
            modal: Modal::None,
            input: String::new(),
        };
        app.refresh();
        app.status = if app.instances.is_empty() {
            "No instances. n new · i import .mrpack · q quit".into()
        } else {
            "↑/↓ select · p play · x stop · n new · c clone · d delete · i import · q quit".into()
        };
        app
    }

    fn refresh(&mut self) {
        self.instances = Instance::all();
        if self.selected >= self.instances.len() {
            self.selected = self.instances.len().saturating_sub(1);
        }
    }

    fn selected_instance(&self) -> Option<&Instance> {
        self.instances.get(self.selected)
    }

    fn next(&mut self) {
        if !self.instances.is_empty() {
            self.selected = (self.selected + 1) % self.instances.len();
        }
    }
    fn prev(&mut self) {
        if !self.instances.is_empty() {
            self.selected = (self.selected + self.instances.len() - 1) % self.instances.len();
        }
    }

    fn game_running(&self) -> bool {
        self.console.as_ref().map(|c| c.is_running()).unwrap_or(false)
    }

    fn auth_session(name: &str) -> AuthSession {
        if let Some(acc) = Account::load() {
            if !acc.mc_access_token.is_empty() {
                return acc.to_session();
            }
        }
        AuthSession::offline(name)
    }

    fn play(&mut self) {
        if self.game_running() {
            self.status = "A game is already running. Press x to stop it first.".into();
            return;
        }
        let Some(inst) = self.instances.get(self.selected).cloned() else { return };
        let host = Host::current();
        let auth = Self::auth_session("Player");
        match inst.launch_argv(&host, &auth) {
            Ok((java, argv)) => {
                let java = java.to_string_lossy().into_owned();
                match PtySession::spawn(&java, &argv, Some(inst.game_dir()), 40, 120) {
                    Ok(session) => {
                        self.console = Some(session);
                        self.status = format!("Launched '{}' ({}).", inst.config.name, auth.user_type);
                        if let Some(i) = self.instances.get_mut(self.selected) {
                            i.mark_played();
                        }
                    }
                    Err(e) => self.status = format!("Failed to start: {e}"),
                }
            }
            Err(e) => self.status = format!("Resolve failed: {e}"),
        }
    }

    fn stop(&mut self) {
        if let Some(c) = &self.console {
            c.kill();
            self.status = "Stopped.".into();
        }
    }

    fn begin(&mut self, modal: Modal) {
        // Guard actions that need a selection / managed instance.
        match modal {
            Modal::CloneName if self.selected_instance().is_none() => return,
            Modal::ConfirmDelete => match self.selected_instance() {
                Some(i) if i.is_managed() => {}
                Some(_) => {
                    self.status = "Can't delete the shared 'main' instance.".into();
                    return;
                }
                None => return,
            },
            _ => {}
        }
        self.input.clear();
        self.modal = modal;
    }

    fn commit_modal(&mut self) {
        let result = match self.modal {
            Modal::NewName => self.commit_new(),
            Modal::CloneName => self.commit_clone(),
            Modal::ImportPath => self.commit_import(),
            Modal::ConfirmDelete => self.commit_delete(),
            Modal::None => Ok(String::new()),
        };
        match result {
            Ok(msg) if !msg.is_empty() => self.status = msg,
            Err(e) => self.status = format!("Error: {e}"),
            _ => {}
        }
        self.modal = Modal::None;
        self.input.clear();
        self.refresh();
    }

    fn commit_new(&mut self) -> anyhow::Result<String> {
        let name = self.input.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("name is required");
        }
        // Template version/mc_home from the current selection, else detect.
        let (version, mc_home, max_mb) = match self.selected_instance() {
            Some(i) => (i.config.version_id.clone(), i.config.mc_home.clone(), i.config.max_mb),
            None => {
                let m = Instance::detect_main().ok_or_else(|| anyhow::anyhow!("no template version available"))?;
                (m.config.version_id, m.config.mc_home, m.config.max_mb)
            }
        };
        let inst = Instance::create(&name, &version, mc_home, max_mb)?;
        Ok(format!("Created instance '{}'", inst.config.name))
    }

    fn commit_clone(&mut self) -> anyhow::Result<String> {
        let new_name = self.input.trim().to_string();
        if new_name.is_empty() {
            anyhow::bail!("name is required");
        }
        let src = self.selected_instance().ok_or_else(|| anyhow::anyhow!("no selection"))?;
        let inst = src.clone_to(&new_name)?;
        Ok(format!("Cloned -> '{}'", inst.config.name))
    }

    fn commit_delete(&mut self) -> anyhow::Result<String> {
        let inst = self.selected_instance().cloned().ok_or_else(|| anyhow::anyhow!("no selection"))?;
        let name = inst.config.name.clone();
        inst.delete()?;
        Ok(format!("Deleted '{name}'"))
    }

    fn commit_import(&mut self) -> anyhow::Result<String> {
        let path = PathBuf::from(self.input.trim());
        if !path.is_file() {
            anyhow::bail!("no such file: {}", path.display());
        }
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "modpack".into());
        let client = Client::new()?;
        // Run the async import to completion on a temporary runtime. This blocks
        // the UI briefly; downloads are cache-fast and the set is small.
        let rt = tokio::runtime::Runtime::new()?;
        let report = rt.block_on(modpack::import_mrpack(
            client.http(),
            &default_cache_dir(),
            &path,
            &name,
            default_mc_dir(),
            4096,
        ))?;
        let warn = if report.version_installed { "" } else { " (⚠ loader not installed)" };
        Ok(format!(
            "Imported '{}' — {} files{}",
            report.instance.config.name, report.installed, warn
        ))
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let w = area.width * percent_x / 100;
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect { x, y, width: w, height }
}

fn ui(f: &mut Frame, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(20)])
        .split(f.area());

    let items: Vec<ListItem> = app
        .instances
        .iter()
        .enumerate()
        .map(|(idx, i)| {
            let running = idx == app.selected && app.game_running();
            let dot = if running { "● " } else { "  " };
            let tag = if i.is_managed() { "" } else { " (shared)" };
            ListItem::new(format!("{dot}{}{}", i.config.name, tag))
        })
        .collect();
    let mut state = ListState::default();
    if !app.instances.is_empty() {
        state.select(Some(app.selected));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" instances "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, cols[0], &mut state);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(cols[1]);

    let title = match app.selected_instance() {
        Some(i) => format!(" console — {} [{}] ", i.config.name, i.config.version_id),
        None => " console ".into(),
    };
    let console_text = match &app.console {
        Some(c) => c.screen_text(),
        None => "No game running. Select an instance and press p to play.".into(),
    };
    let console = Paragraph::new(console_text)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    f.render_widget(console, right[0]);

    let status = Paragraph::new(app.status.as_str())
        .block(Block::default().borders(Borders::ALL).title(" status "));
    f.render_widget(status, right[1]);

    // Modal overlay.
    if app.modal != Modal::None {
        let (title, body) = match app.modal {
            Modal::NewName => (" new instance ", format!("Name: {}_", app.input)),
            Modal::CloneName => (" clone instance ", format!("New name: {}_", app.input)),
            Modal::ImportPath => (" import .mrpack ", format!("Path: {}_", app.input)),
            Modal::ConfirmDelete => {
                let n = app.selected_instance().map(|i| i.config.name.as_str()).unwrap_or("");
                (" confirm delete ", format!("Delete '{n}'? Enter = yes, Esc = no"))
            }
            Modal::None => ("", String::new()),
        };
        let area = centered_rect(60, 5, f.area());
        f.render_widget(Clear, area);
        let popup = Paragraph::new(body)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false });
        f.render_widget(popup, area);
    }
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    let mut app = App::new();
    loop {
        terminal.draw(|f| ui(f, &app))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                // Modal input takes precedence.
                if app.modal != Modal::None {
                    match key.code {
                        KeyCode::Esc => {
                            app.modal = Modal::None;
                            app.input.clear();
                        }
                        KeyCode::Enter => app.commit_modal(),
                        KeyCode::Backspace => {
                            app.input.pop();
                        }
                        KeyCode::Char(c) if app.modal != Modal::ConfirmDelete => app.input.push(c),
                        _ => {}
                    }
                    continue;
                }

                let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
                if ctrl_c {
                    break;
                }
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Up | KeyCode::Char('k') => app.prev(),
                    KeyCode::Enter | KeyCode::Char('p') => app.play(),
                    KeyCode::Char('x') => app.stop(),
                    KeyCode::Char('n') => app.begin(Modal::NewName),
                    KeyCode::Char('c') => app.begin(Modal::CloneName),
                    KeyCode::Char('d') => app.begin(Modal::ConfirmDelete),
                    KeyCode::Char('i') => app.begin(Modal::ImportPath),
                    _ => {}
                }
            }
        }
    }
    if let Some(c) = &app.console {
        c.kill();
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}
