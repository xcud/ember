//! ember TUI (MVP).
//!
//! Lists instances, launches one, and streams its log into a console pane via a
//! PTY (`ember-term`). This is the spine that grows into the multiplexed
//! companion shell (game console + shell + AI chat).

use std::io::Stdout;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use ember_term::PtySession;
use launcher_core::auth::Account;
use launcher_core::instance::Instance;
use launcher_core::launch::{AuthSession, Host};

struct App {
    instances: Vec<Instance>,
    selected: usize,
    console: Option<PtySession>,
    status: String,
}

impl App {
    fn new() -> Self {
        let mut instances = Instance::list();
        if instances.is_empty() {
            // Bootstrap from an existing ~/.minecraft if we can.
            if let Some(main) = Instance::detect_main() {
                instances.push(main);
            }
        }
        let status = if instances.is_empty() {
            "No instances found. (Create one with the CLI; bootstrap needs ~/.minecraft)".into()
        } else {
            "Ready. ↑/↓ select · p play · x stop · q quit".into()
        };
        App { instances, selected: 0, console: None, status }
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

    /// Pick a launch session: cached account if still valid (no network), else
    /// offline. (Online refresh waits on Microsoft API approval anyway.)
    fn auth_session(name: &str) -> AuthSession {
        if let Some(acc) = Account::load() {
            // Use the cached token only if it's clearly still valid; never block
            // the UI on a network refresh here.
            if !acc.mc_access_token.is_empty() {
                return acc.to_session();
            }
        }
        AuthSession::offline(name)
    }

    fn play(&mut self) {
        if self.console.as_ref().map(|c| c.is_running()).unwrap_or(false) {
            self.status = "A game is already running. Press x to stop it first.".into();
            return;
        }
        let Some(inst) = self.instances.get(self.selected).cloned() else {
            return;
        };
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
}

fn ui(f: &mut Frame, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(20)])
        .split(f.area());

    // Instance list.
    let items: Vec<ListItem> = app
        .instances
        .iter()
        .map(|i| {
            let running = app
                .console
                .as_ref()
                .map(|c| c.is_running())
                .unwrap_or(false);
            let dot = if running { "● " } else { "  " };
            ListItem::new(format!("{dot}{}  [{}]", i.config.name, i.config.version_id))
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

    // Right side: console + status.
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(cols[1]);

    let title = match app.selected_instance() {
        Some(i) => format!(" console — {} ", i.config.name),
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
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    let mut app = App::new();
    loop {
        terminal.draw(|f| ui(f, &app))?;
        // Poll briefly so the console pane refreshes while idle.
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c');
                match key.code {
                    KeyCode::Char('q') if !ctrl_c => break,
                    _ if ctrl_c => break,
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Up | KeyCode::Char('k') => app.prev(),
                    KeyCode::Enter | KeyCode::Char('p') => app.play(),
                    KeyCode::Char('x') => app.stop(),
                    _ => {}
                }
            }
        }
    }
    // Clean up a running child on exit.
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
