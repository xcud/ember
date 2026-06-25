//! ember TUI.
//!
//! Left: instance list. Right: a switchable detail pane showing either the
//! selected instance's **Mods** or the running game's **Console** (a PTY-backed
//! virtual terminal via `ember-term`). Focus moves between panes with Tab; the
//! console resizes to its pane and scrolls. Instance management: new, clone,
//! delete, import `.mrpack`.

use std::collections::HashSet;
use std::io::Stdout;
use std::path::PathBuf;
use std::sync::mpsc;
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
use launcher_core::manifest::ContentType;
use launcher_core::modpack;
use launcher_core::modrinth::{Client, SearchHit};

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

/// A row in the Mods list: a jar on disk, enriched from the lock when known.
struct ModRow {
    name: String,    // title or slug, else filename
    version: String, // version number, if known
    filename: String,
    size: u64,
    description: String,
}

struct App {
    instances: Vec<Instance>,
    selected: usize,
    content_type: ContentType,
    mods: Vec<ModRow>,
    mod_state: ListState,
    console: Option<PtySession>,
    console_scroll: usize,
    focus: Focus,
    right_view: RightView,
    status: String,
    modal: Modal,
    input: String,
    // Add-mod results picker.
    results: Vec<SearchHit>,
    result_state: ListState,
    picking: bool,
    // Background metadata enrichment (resolve + describe an instance's mods).
    enrich_tx: mpsc::Sender<String>,
    enrich_rx: mpsc::Receiver<String>,
    enrich_started: HashSet<String>,
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

fn human_size(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.0} KB", bytes as f64 / (1u64 << 10) as f64)
    } else {
        format!("{bytes} B")
    }
}

fn human_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn content_next(ct: ContentType) -> ContentType {
    match ct {
        ContentType::Mod => ContentType::ResourcePack,
        ContentType::ResourcePack => ContentType::Shader,
        ContentType::Shader => ContentType::Mod,
    }
}
fn content_prev(ct: ContentType) -> ContentType {
    match ct {
        ContentType::Mod => ContentType::Shader,
        ContentType::ResourcePack => ContentType::Mod,
        ContentType::Shader => ContentType::ResourcePack,
    }
}

/// Rows for the given content type. Mods are enriched from the lock; resource
/// packs and shaders are listed (as zips) straight from their folder.
fn list_content(inst: &Instance, ct: ContentType) -> Vec<ModRow> {
    if ct == ContentType::Mod {
        return list_mods(inst);
    }
    let dir = inst.config.game_dir.join(ct.dir_name());
    let mut rows = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let filename = e.file_name().to_string_lossy().into_owned();
            if !(filename.ends_with(".zip") || filename.ends_with(".jar")) {
                continue;
            }
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            rows.push(ModRow { name: filename.clone(), version: String::new(), filename, size, description: String::new() });
        }
    }
    rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    rows
}

/// Build the mod rows for an instance: jars on disk, enriched from the lock.
fn list_mods(inst: &Instance) -> Vec<ModRow> {
    use std::collections::HashMap;
    let locked: HashMap<String, launcher_core::manifest::LockedMod> =
        launcher_core::sync::load_lock(&inst.lock_path())
            .map(|l| l.mods.into_iter().map(|m| (m.filename.clone(), m)).collect())
            .unwrap_or_default();

    let mut rows = Vec::new();
    if let Ok(rd) = std::fs::read_dir(inst.mods_dir()) {
        for e in rd.flatten() {
            let filename = e.file_name().to_string_lossy().into_owned();
            if !filename.ends_with(".jar") {
                continue;
            }
            let size_on_disk = e.metadata().map(|m| m.len()).unwrap_or(0);
            match locked.get(&filename) {
                Some(m) => rows.push(ModRow {
                    name: if m.title.is_empty() { m.slug.clone() } else { m.title.clone() },
                    version: m.version_number.clone(),
                    filename,
                    size: if m.size > 0 { m.size } else { size_on_disk },
                    description: m.description.clone(),
                }),
                None => rows.push(ModRow {
                    name: filename.clone(),
                    version: String::new(),
                    filename,
                    size: size_on_disk,
                    description: String::new(),
                }),
            }
        }
    }
    rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    rows
}

impl App {
    fn new() -> Self {
        let (enrich_tx, enrich_rx) = mpsc::channel();
        let mut app = App {
            instances: Vec::new(),
            selected: 0,
            content_type: ContentType::Mod,
            mods: Vec::new(),
            mod_state: ListState::default(),
            console: None,
            console_scroll: 0,
            focus: Focus::List,
            right_view: RightView::Properties,
            status: String::new(),
            modal: Modal::None,
            input: String::new(),
            results: Vec::new(),
            result_state: ListState::default(),
            picking: false,
            enrich_tx,
            enrich_rx,
            enrich_started: HashSet::new(),
        };
        app.refresh();
        app.status =
            "Tab/←→ focus · 1/2/3 tabs · [ ] content type · p play · a add · r remove · u update · q quit".into();
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
        let ct = self.content_type;
        self.mods = self.instances.get(self.selected).map(|i| list_content(i, ct)).unwrap_or_default();
        if self.mods.is_empty() {
            self.mod_state.select(None);
        } else {
            self.mod_state.select(Some(0));
        }
        self.maybe_enrich();
    }

    /// Kick off background metadata enrichment for the selected instance, once.
    /// Resolves its mods to a real pack (with titles/descriptions) off-thread,
    /// then the UI re-reads the enriched lock when the worker signals done.
    fn maybe_enrich(&mut self) {
        // Only mods carry lock metadata; resource packs/shaders are folder-listed.
        if self.content_type != ContentType::Mod {
            return;
        }
        let Some(inst) = self.selected_instance().cloned() else { return };
        // Skip if there's nothing to enrich or we've already started it.
        if inst.mods_dir().read_dir().map(|mut d| d.next().is_none()).unwrap_or(true) {
            return;
        }
        let key = inst.config.name.clone();
        if !self.enrich_started.insert(key.clone()) {
            return;
        }
        let tx = self.enrich_tx.clone();
        std::thread::spawn(move || {
            if let (Ok(client), Ok(rt)) = (Client::new(), tokio::runtime::Runtime::new()) {
                let _ = rt.block_on(manage::ensure_pack(&client, &inst));
            }
            let _ = tx.send(key);
        });
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

    /// Run the search and open the results picker (does not install yet).
    fn commit_add(&mut self) -> anyhow::Result<String> {
        let query = self.input.trim().to_string();
        if query.is_empty() {
            anyhow::bail!("search query required");
        }
        let inst = self.selected_instance().cloned().ok_or_else(|| anyhow::anyhow!("no instance selected"))?;
        let client = Client::new()?;
        let rt = tokio::runtime::Runtime::new()?;
        let hits = rt.block_on(manage::search_content(&client, &inst, self.content_type, &query))?;
        if hits.is_empty() {
            return Ok(format!("No {} results for '{query}'", self.content_type.label()));
        }
        let n = hits.len();
        self.results = hits;
        self.result_state.select(Some(0));
        self.picking = true;
        Ok(format!("{n} results — ↑/↓ choose · Enter install · Esc cancel"))
    }

    fn result_next(&mut self) {
        if self.results.is_empty() {
            return;
        }
        let i = self.result_state.selected().unwrap_or(0);
        self.result_state.select(Some((i + 1).min(self.results.len() - 1)));
    }
    fn result_prev(&mut self) {
        let i = self.result_state.selected().unwrap_or(0);
        self.result_state.select(Some(i.saturating_sub(1)));
    }
    fn cancel_picker(&mut self) {
        self.picking = false;
        self.results.clear();
        self.status = "Cancelled.".into();
    }

    fn install_selected_result(&mut self) {
        let Some(idx) = self.result_state.selected() else { return };
        let Some(hit) = self.results.get(idx).cloned() else { return };
        let Some(inst) = self.selected_instance().cloned() else { return };
        self.picking = false;
        self.results.clear();
        self.status = format!("Installing {} ...", hit.title);
        let ct = self.content_type;
        let run = (|| -> anyhow::Result<manage::AddReport> {
            let client = Client::new()?;
            let rt = tokio::runtime::Runtime::new()?;
            Ok(rt.block_on(manage::add_content(&client, &default_cache_dir(), &inst, ct, &hit.slug))?)
        })();
        self.status = match run {
            Ok(r) if r.installed.is_empty() => format!("'{}' already present", hit.title),
            Ok(r) => format!("Added: {}", r.installed.join(", ")),
            Err(e) => format!("Add failed: {e}"),
        };
        self.refresh_mods();
        self.right_view = RightView::Mods;
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
        let Some(row) = self.mods.get(idx) else { return };
        let filename = row.filename.clone();
        let label = row.name.clone();
        self.status = match manage::remove_content(&inst, self.content_type, &filename) {
            Ok(()) => format!("Removed {label}"),
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
    let tabs = Tabs::new(vec!["1 Properties", "2 Content", "3 Console"])
        .select(app.right_view.index())
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .divider("│");
    f.render_widget(tabs, right[0]);

    let content = right[1];
    match app.right_view {
        RightView::Mods => {
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(6)])
                .split(content);

            // Content-type selector strip — all three visible, active highlighted.
            let ct_index = ContentType::ALL.iter().position(|c| *c == app.content_type).unwrap_or(0);
            let ct_titles: Vec<String> = ContentType::ALL
                .iter()
                .map(|c| format!("{} {}", if *c == app.content_type { "▸" } else { " " }, c.label()))
                .collect();
            let ct_tabs = Tabs::new(ct_titles)
                .select(ct_index)
                .style(Style::default().fg(Color::Gray))
                .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
                .divider("│");
            f.render_widget(ct_tabs, split[0]);

            let title = format!(" {} — {inst_name} ({}) · [ ] switch type ", app.content_type.label(), app.mods.len());
            let items: Vec<ListItem> = app
                .mods
                .iter()
                .map(|m| {
                    let ver = if m.version.is_empty() {
                        String::new()
                    } else {
                        format!("  v{}", m.version)
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(m.name.clone()),
                        Span::styled(ver, Style::default().fg(Color::DarkGray)),
                    ]))
                })
                .collect();
            let widget = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(focused_border(right_focused))
                        .title(title),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▸ ");
            f.render_stateful_widget(widget, split[1], &mut app.mod_state);

            // Detail box for the highlighted item.
            let detail = match app.mod_state.selected().and_then(|i| app.mods.get(i)) {
                Some(m) => {
                    let desc = if m.description.is_empty() { "—" } else { m.description.as_str() };
                    let ver = if m.version.is_empty() { String::new() } else { format!("  v{}", m.version) };
                    format!("{}{}\n{}\n\nfile: {}  ({})", m.name, ver, desc, m.filename, human_size(m.size))
                }
                None => format!("No {}. Press a to add.", app.content_type.label().to_lowercase()),
            };
            let detail_widget = Paragraph::new(detail)
                .block(Block::default().borders(Borders::ALL).title(" details "))
                .wrap(Wrap { trim: true });
            f.render_widget(detail_widget, split[2]);
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

    // Add-mod results picker overlay (list + detail).
    if app.picking {
        let full = f.area();
        let area = centered_rect(78, full.height.saturating_sub(6).max(10), full);
        f.render_widget(Clear, area);
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(6)])
            .split(area);

        let items: Vec<ListItem> = app
            .results
            .iter()
            .map(|hit| {
                ListItem::new(Line::from(vec![
                    Span::styled(hit.title.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("  ↓{}", human_count(hit.downloads)),
                        Style::default().fg(Color::Green),
                    ),
                    Span::styled(format!("  by {}", hit.author), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let widget = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" choose a mod — ↑/↓ · Enter install · Esc cancel "),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▸ ");
        f.render_stateful_widget(widget, split[0], &mut app.result_state);

        let detail = match app.result_state.selected().and_then(|i| app.results.get(i)) {
            Some(hit) => format!(
                "{}\n{}\n\n{} downloads · by {} · {}",
                hit.title,
                hit.description,
                human_count(hit.downloads),
                hit.author,
                hit.categories.join(", "),
            ),
            None => String::new(),
        };
        let detail_widget = Paragraph::new(detail)
            .block(Block::default().borders(Borders::ALL).title(" details "))
            .wrap(Wrap { trim: true });
        f.render_widget(detail_widget, split[1]);
    }
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    let mut app = App::new();
    loop {
        // Apply any completed background enrichment for the visible instance.
        while let Ok(name) = app.enrich_rx.try_recv() {
            if app.content_type == ContentType::Mod
                && app.selected_instance().map(|i| i.config.name == name).unwrap_or(false)
            {
                let sel = app.mod_state.selected();
                let ct = app.content_type;
                app.mods = app.instances.get(app.selected).map(|i| list_content(i, ct)).unwrap_or_default();
                app.mod_state.select(sel.filter(|i| *i < app.mods.len()).or(Some(0)).filter(|_| !app.mods.is_empty()));
            }
        }

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

                // Results picker takes precedence over normal keys.
                if app.picking {
                    match key.code {
                        KeyCode::Esc => app.cancel_picker(),
                        KeyCode::Down | KeyCode::Char('j') => app.result_next(),
                        KeyCode::Up | KeyCode::Char('k') => app.result_prev(),
                        KeyCode::Enter => app.install_selected_result(),
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
                    KeyCode::Char(']') => {
                        app.content_type = content_next(app.content_type);
                        app.right_view = RightView::Mods;
                        app.refresh_mods();
                    }
                    KeyCode::Char('[') => {
                        app.content_type = content_prev(app.content_type);
                        app.right_view = RightView::Mods;
                        app.refresh_mods();
                    }
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
