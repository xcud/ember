//! ember TUI.
//!
//! Left: instance list. Right: a switchable detail pane showing either the
//! selected instance's **Mods** or the running game's **Console** (a PTY-backed
//! virtual terminal via `ember-term`). Focus moves between panes with Tab; the
//! console resizes to its pane and scrolls. Instance management: new, clone,
//! delete, import `.mrpack`.

use std::io::Stdout;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use ember_term::PtySession;
use launcher_core::auth::Account;
use launcher_core::instance::Instance;
use launcher_core::launch::{AuthSession, Host};
use launcher_core::manage;
use launcher_core::modpack;
use launcher_core::modrinth::Client;

const SIDEBAR_W: u16 = 34;
const STATUS_H: u16 = 3;
const TABBAR_H: u16 = 1;

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    List,
    Right,
}

#[derive(PartialEq, Clone, Copy)]
enum RightView {
    Mods,
    Console,
    Properties,
}

impl RightView {
    fn index(self) -> usize {
        match self {
            RightView::Properties => 0,
            RightView::Mods => 1,
            RightView::Console => 2,
        }
    }
    fn cycle(self) -> RightView {
        match self {
            RightView::Properties => RightView::Mods,
            RightView::Mods => RightView::Console,
            RightView::Console => RightView::Properties,
        }
    }
}

#[derive(PartialEq)]
enum Modal {
    None,
    NewName,
    CloneName,
    ImportPath,
    ConfirmDelete,
    AddMod,
}

struct App {
    instances: Vec<Instance>,
    selected: usize,
    mods: Vec<String>,
    mod_state: ListState,
    console: Option<PtySession>,
    console_scroll: usize,
    focus: Focus,
    right_view: RightView,
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

fn list_mods(inst: &Instance) -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(inst.mods_dir()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(".jar") {
                v.push(n);
            }
        }
    }
    v.sort();
    v
}

impl App {
    fn new() -> Self {
        let mut app = App {
            instances: Vec::new(),
            selected: 0,
            mods: Vec::new(),
            mod_state: ListState::default(),
            console: None,
            console_scroll: 0,
            focus: Focus::List,
            right_view: RightView::Properties,
            status: String::new(),
            modal: Modal::None,
            input: String::new(),
        };
        app.refresh();
        app.status =
            "Tab/←→ focus · Esc back · 1/2/3 tabs · p play · a add · r remove · u update · n/c/d/i instance · q quit".into();
        app
    }

    fn refresh(&mut self) {
        self.instances = Instance::all();
        if self.selected >= self.instances.len() {
            self.selected = self.instances.len().saturating_sub(1);
        }
        self.refresh_mods();
    }

    fn refresh_mods(&mut self) {
        self.mods = self.instances.get(self.selected).map(list_mods).unwrap_or_default();
        if self.mods.is_empty() {
            self.mod_state.select(None);
        } else {
            self.mod_state.select(Some(0));
        }
    }

    fn selected_instance(&self) -> Option<&Instance> {
        self.instances.get(self.selected)
    }

    fn game_running(&self) -> bool {
        self.console.as_ref().map(|c| c.is_running()).unwrap_or(false)
    }

    fn select_next(&mut self) {
        if !self.instances.is_empty() {
            self.selected = (self.selected + 1) % self.instances.len();
            self.refresh_mods();
        }
    }
    fn select_prev(&mut self) {
        if !self.instances.is_empty() {
            self.selected = (self.selected + self.instances.len() - 1) % self.instances.len();
            self.refresh_mods();
        }
    }

    fn mod_next(&mut self) {
        if self.mods.is_empty() {
            return;
        }
        let i = self.mod_state.selected().unwrap_or(0);
        self.mod_state.select(Some((i + 1).min(self.mods.len() - 1)));
    }
    fn mod_prev(&mut self) {
        if self.mods.is_empty() {
            return;
        }
        let i = self.mod_state.selected().unwrap_or(0);
        self.mod_state.select(Some(i.saturating_sub(1)));
    }

    fn set_console_scroll(&mut self, n: usize) {
        self.console_scroll = n.min(2000);
        if let Some(c) = &self.console {
            c.set_scrollback(self.console_scroll);
        }
    }

    fn console_scroll_by(&mut self, delta: isize) {
        let new = (self.console_scroll as isize + delta).max(0) as usize;
        self.set_console_scroll(new);
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
                        self.console_scroll = 0;
                        self.right_view = RightView::Console;
                        self.focus = Focus::Right;
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
        match modal {
            Modal::CloneName if self.selected_instance().is_none() => return,
            Modal::ConfirmDelete => match self.selected_instance() {
                Some(i) if i.config.linked => {
                    self.status = "Can't delete a linked instance (it points at your real install).".into();
                    return;
                }
                Some(i) if i.is_managed() => {}
                _ => return,
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
            Modal::AddMod => self.commit_add(),
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
        let (version, mc_home, max_mb) = match self.selected_instance() {
            Some(i) => (i.config.version_id.clone(), i.config.mc_home.clone(), i.config.max_mb),
            None => {
                let m = Instance::ensure_main().ok_or_else(|| anyhow::anyhow!("no template version available"))?;
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
        Ok(format!("Imported '{}' — {} files{}", report.instance.config.name, report.installed, warn))
    }

    fn commit_add(&mut self) -> anyhow::Result<String> {
        let query = self.input.trim().to_string();
        if query.is_empty() {
            anyhow::bail!("search query required");
        }
        let inst = self.selected_instance().cloned().ok_or_else(|| anyhow::anyhow!("no instance selected"))?;
        let client = Client::new()?;
        let rt = tokio::runtime::Runtime::new()?;
        let report = rt.block_on(manage::add_mod(&client, &default_cache_dir(), &inst, &query))?;
        if report.installed.is_empty() {
            Ok(format!("'{query}': nothing new installed (already present?)"))
        } else {
            Ok(format!("Added: {}", report.installed.join(", ")))
        }
    }

    fn update_instance(&mut self) {
        let Some(inst) = self.selected_instance().cloned() else { return };
        self.status = format!("Updating '{}' ...", inst.config.name);
        let run = (|| -> anyhow::Result<manage::UpdateSummary> {
            let client = Client::new()?;
            let rt = tokio::runtime::Runtime::new()?;
            Ok(rt.block_on(manage::update_instance(&client, &default_cache_dir(), &inst))?)
        })();
        self.status = match run {
            Ok(s) => format!(
                "'{}': {} updated, {} added, {} incompatible, {} downloaded",
                inst.config.name, s.updated, s.added, s.incompatible, s.downloaded
            ),
            Err(e) => format!("Update failed: {e}"),
        };
        self.refresh_mods();
    }

    fn remove_selected_mod(&mut self) {
        if self.right_view != RightView::Mods {
            return;
        }
        let Some(inst) = self.selected_instance().cloned() else { return };
        let Some(idx) = self.mod_state.selected() else { return };
        let Some(name) = self.mods.get(idx).cloned() else { return };
        self.status = match manage::remove_mod(&inst, &name) {
            Ok(()) => format!("Removed {name}"),
            Err(e) => format!("Remove failed: {e}"),
        };
        self.refresh_mods();
    }

    /// Resize the console PTY to match its pane's inner dimensions.
    fn fit_console(&mut self, term: Size) {
        let Some(c) = self.console.as_mut() else { return };
        let cols = term.width.saturating_sub(SIDEBAR_W + 2).max(1);
        let rows = term.height.saturating_sub(TABBAR_H + STATUS_H + 2).max(1);
        if c.size() != (rows, cols) {
            c.resize(rows, cols);
        }
    }
}

fn human_ago(secs: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(secs);
    let d = now.saturating_sub(secs);
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let w = area.width * percent_x / 100;
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect { x, y, width: w, height }
}

fn focused_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_W), Constraint::Min(20)])
        .split(f.area());

    // Instance list.
    let items: Vec<ListItem> = app
        .instances
        .iter()
        .enumerate()
        .map(|(idx, i)| {
            let running = idx == app.selected && app.game_running();
            let dot = if running { "● " } else { "  " };
            let tag = if i.config.linked { " (linked)" } else { "" };
            ListItem::new(format!("{dot}{}{}", i.config.name, tag))
        })
        .collect();
    let mut list_state = ListState::default();
    if !app.instances.is_empty() {
        list_state.select(Some(app.selected));
    }
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(focused_border(app.focus == Focus::List))
                .title(" instances "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, cols[0], &mut list_state);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(TABBAR_H),
            Constraint::Min(3),
            Constraint::Length(STATUS_H),
        ])
        .split(cols[1]);

    let inst_name = app.selected_instance().map(|i| i.config.name.clone()).unwrap_or_default();
    let right_focused = app.focus == Focus::Right;

    // Tab strip.
    let tabs = Tabs::new(vec!["1 Properties", "2 Mods", "3 Console"])
        .select(app.right_view.index())
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .divider("│");
    f.render_widget(tabs, right[0]);

    let content = right[1];
    match app.right_view {
        RightView::Mods => {
            let title = format!(" mods — {inst_name} ({}) ", app.mods.len());
            let items: Vec<ListItem> = app.mods.iter().map(|m| ListItem::new(m.as_str())).collect();
            let widget = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(focused_border(right_focused))
                        .title(title),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▸ ");
            f.render_stateful_widget(widget, content, &mut app.mod_state);
        }
        RightView::Console => {
            let scroll_tag = if app.console_scroll > 0 {
                format!(" [↑{}] ", app.console_scroll)
            } else {
                String::new()
            };
            let title = format!(" console — {inst_name}{scroll_tag} ");
            if let Some(c) = &app.console {
                c.set_scrollback(app.console_scroll);
            }
            let text = match &app.console {
                Some(c) => c.screen_text(),
                None => "No game running. Select an instance and press p to play.".into(),
            };
            let widget = Paragraph::new(text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(focused_border(right_focused))
                        .title(title),
                )
                .wrap(Wrap { trim: false });
            f.render_widget(widget, content);
        }
        RightView::Properties => {
            let text = match app.selected_instance() {
                Some(i) => {
                    let last = i.config.last_played.map(human_ago).unwrap_or_else(|| "never".into());
                    let link = if i.config.linked {
                        format!("yes → {}", i.config.game_dir.display())
                    } else {
                        "no".into()
                    };
                    format!(
                        "Name:           {}\n\
                         Version:        {}\n\
                         Linked:         {}\n\
                         Game dir:       {}\n\
                         Shared install: {}\n\
                         Max RAM:        {} MB\n\
                         Mods:           {}\n\
                         Last played:    {}",
                        i.config.name,
                        i.config.version_id,
                        link,
                        i.config.game_dir.display(),
                        i.config.mc_home.display(),
                        i.config.max_mb,
                        app.mods.len(),
                        last,
                    )
                }
                None => "No instance selected.".into(),
            };
            let widget = Paragraph::new(text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(focused_border(right_focused))
                        .title(format!(" properties — {inst_name} ")),
                )
                .wrap(Wrap { trim: false });
            f.render_widget(widget, content);
        }
    }

    let status = Paragraph::new(app.status.as_str())
        .block(Block::default().borders(Borders::ALL).title(" status "));
    f.render_widget(status, right[2]);

    if app.modal != Modal::None {
        let (title, body) = match app.modal {
            Modal::NewName => (" new instance ", format!("Name: {}_", app.input)),
            Modal::CloneName => (" clone instance ", format!("New name: {}_", app.input)),
            Modal::ImportPath => (" import .mrpack ", format!("Path: {}_", app.input)),
            Modal::ConfirmDelete => {
                let n = app.selected_instance().map(|i| i.config.name.as_str()).unwrap_or("");
                (" confirm delete ", format!("Delete '{n}'? Enter = yes, Esc = no"))
            }
            Modal::AddMod => (" add mod (Modrinth) ", format!("Search: {}_", app.input)),
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
        if let Ok(size) = terminal.size() {
            app.fit_console(size);
        }
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
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

                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    break;
                }

                // Focus-dependent vertical navigation / scrolling.
                let right_console = app.focus == Focus::Right && app.right_view == RightView::Console;
                let right_mods = app.focus == Focus::Right && app.right_view == RightView::Mods;
                match key.code {
                    KeyCode::Char('q') => break,
                    // Esc / ← always back out to the instance list.
                    KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => app.focus = Focus::List,
                    KeyCode::Right | KeyCode::Char('l') => app.focus = Focus::Right,
                    KeyCode::Tab => {
                        app.focus = if app.focus == Focus::List { Focus::Right } else { Focus::List };
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if right_console {
                            app.console_scroll_by(-1);
                        } else if right_mods {
                            app.mod_next();
                        } else {
                            app.select_next();
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if right_console {
                            app.console_scroll_by(1);
                        } else if right_mods {
                            app.mod_prev();
                        } else {
                            app.select_prev();
                        }
                    }
                    KeyCode::PageDown => app.console_scroll_by(-10),
                    KeyCode::PageUp => app.console_scroll_by(10),
                    KeyCode::Home => {
                        if right_console {
                            app.set_console_scroll(2000); // oldest; vt100 clamps to history
                        } else if right_mods && !app.mods.is_empty() {
                            app.mod_state.select(Some(0));
                        } else if !app.instances.is_empty() {
                            app.selected = 0;
                            app.refresh_mods();
                        }
                    }
                    KeyCode::End => {
                        if right_console {
                            app.set_console_scroll(0); // live bottom
                        } else if right_mods && !app.mods.is_empty() {
                            app.mod_state.select(Some(app.mods.len() - 1));
                        } else if !app.instances.is_empty() {
                            app.selected = app.instances.len() - 1;
                            app.refresh_mods();
                        }
                    }
                    KeyCode::Char('m') => {
                        app.right_view = RightView::Mods;
                        app.focus = Focus::Right;
                    }
                    KeyCode::Char('1') => app.right_view = RightView::Properties,
                    KeyCode::Char('2') => app.right_view = RightView::Mods,
                    KeyCode::Char('3') => app.right_view = RightView::Console,
                    KeyCode::Char('v') => app.right_view = app.right_view.cycle(),
                    KeyCode::Enter | KeyCode::Char('p') => app.play(),
                    KeyCode::Char('x') => app.stop(),
                    KeyCode::Char('n') => app.begin(Modal::NewName),
                    KeyCode::Char('c') => app.begin(Modal::CloneName),
                    KeyCode::Char('d') => app.begin(Modal::ConfirmDelete),
                    KeyCode::Char('i') => app.begin(Modal::ImportPath),
                    KeyCode::Char('a') | KeyCode::Char('/') => app.begin(Modal::AddMod),
                    KeyCode::Char('u') => app.update_instance(),
                    KeyCode::Delete | KeyCode::Char('r') => app.remove_selected_mod(),
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
