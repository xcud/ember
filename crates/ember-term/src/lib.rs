//! PTY-backed terminal sessions.
//!
//! Spawns a process in a real pseudo-terminal (ConPTY on Windows, `forkpty`/
//! `openpty` on Unix via `portable-pty`) and pumps its output through a `vt100`
//! emulator, so the live screen can be both rendered and queried. This is the
//! same pattern as `lit-bridge-rs`: one PTY output stream, branched to a parser
//! for observation and available as a grid for display.
//!
//! For ember's MVP a session hosts the running game's log stream; the same type
//! will back the multiplexed shell (game console + shell + AI chat) later.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// A process running in a pseudo-terminal, with its screen observable via vt100.
pub struct PtySession {
    parser: Arc<Mutex<vt100::Parser>>,
    running: Arc<AtomicBool>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rows: u16,
    cols: u16,
}

impl PtySession {
    /// Spawn `program` with `args` in a `rows`x`cols` PTY. A background thread
    /// streams output into the vt100 parser until the process exits.
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        if let Some(c) = cwd {
            cmd.cwd(c);
        }

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave); // close our handle to the slave so EOF propagates

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 2000)));
        let running = Arc::new(AtomicBool::new(true));

        let parser_rx = parser.clone();
        let running_rx = running.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut p) = parser_rx.lock() {
                            p.process(&buf[..n]);
                        }
                    }
                    Err(_) => break,
                }
            }
            running_rx.store(false, Ordering::SeqCst);
        });

        Ok(Self {
            parser,
            running,
            child: Arc::new(Mutex::new(child)),
            master: pair.master,
            writer,
            rows,
            cols,
        })
    }

    /// The current visible screen as text (rows joined by newlines).
    pub fn screen_text(&self) -> String {
        self.parser
            .lock()
            .map(|p| p.screen().contents())
            .unwrap_or_default()
    }

    /// Is the child still running?
    pub fn is_running(&self) -> bool {
        if !self.running.load(Ordering::SeqCst) {
            return false;
        }
        if let Ok(mut c) = self.child.lock() {
            if matches!(c.try_wait(), Ok(Some(_))) {
                return false;
            }
        }
        true
    }

    /// Feed bytes to the process's stdin (used by the interactive shell/AI panes).
    pub fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)
    }

    /// Resize the PTY and emulator (call when the pane size changes).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.lock() {
            p.set_size(rows, cols);
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Terminate the child.
    pub fn kill(&self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_process_output() {
        let session = PtySession::spawn(
            "bash",
            &["-lc".into(), "printf 'hello-from-pty\\n'; sleep 0.3".into()],
            None,
            24,
            80,
        )
        .expect("spawn pty");
        // Give the reader thread time to consume the output.
        thread::sleep(std::time::Duration::from_millis(500));
        let text = session.screen_text();
        assert!(
            text.contains("hello-from-pty"),
            "screen did not capture output, got: {text:?}"
        );
    }
}
