// Copyright 2024 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::panic::{self, AssertUnwindSafe};

use shpool_protocol::TtySize;
use tracing::{error, info};

use crate::{
    config::{self, SessionRestoreEngine, SessionRestoreMode},
    tty::{TtySizeExt as _, DEFAULT_TTY_SIZE},
};

// To prevent data getting dropped, we set this to be large, but we don't want
// to use u16::MAX, since the vt100 crate eagerly fills in its rows, and doing
// so is very memory intensive. The right fix is to get the vt100 crate to
// lazily initialize its rows, but that is likely a bunch of work.
const VTERM_WIDTH: u16 = 1024;

/// Some session shpool specific config getters
trait ConfigExt {
    /// Effective vterm width.
    ///
    /// See also `VTERM_WIDTH`.
    fn vterm_width(&self) -> u16;
}

impl ConfigExt for config::Manager {
    fn vterm_width(&self) -> u16 {
        let config = self.get();
        config.vt100_output_spool_width.unwrap_or(VTERM_WIDTH)
    }
}

pub trait SessionSpool {
    /// Resizes the internal representation to new tty size.
    fn resize(&mut self, size: TtySize);

    /// Gets a byte sequence to restore the on-screen session content.
    ///
    /// The returned sequence is expected to be able to restore the screen
    /// content regardless of any prior screen state. It thus mostly likely
    /// includes some terminal control codes to reset the screen from any
    /// state back to a known good state.
    ///
    /// Note that what exactly is restored is determined by the implementation,
    /// and thus can vary from do nothing to a few lines to a full screen,
    /// etc.
    fn restore_buffer(&self) -> Vec<u8>;

    /// Process bytes from pty master.
    fn process(&mut self, bytes: &[u8]);
}

/// A spool that doesn't do anything.
pub struct NullSpool;
impl SessionSpool for NullSpool {
    fn resize(&mut self, _: TtySize) {}

    fn restore_buffer(&self) -> Vec<u8> {
        info!("generating null restore buf");
        vec![]
    }

    fn process(&mut self, _: &[u8]) {}
}

/// A spool that restores the last screenful of content using shpool_vt100.
pub struct Vt100Screen {
    parser: shpool_vt100::Parser,
    /// Other options will be read dynamically from config.
    config: config::Manager,
}

impl SessionSpool for Vt100Screen {
    fn resize(&mut self, size: TtySize) {
        self.parser.screen_mut().set_size(size.rows, self.config.vterm_width());
    }

    fn restore_buffer(&self) -> Vec<u8> {
        let (rows, cols) = self.parser.screen().size();
        info!("computing screen restore buf with (rows={}, cols={})", rows, cols);
        self.parser.screen().contents_formatted()
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes)
    }
}

/// A spool that restores the last n lines of content using shpool_vt100.
pub struct Vt100Lines {
    parser: shpool_vt100::Parser,
    /// How many lines to restore
    nlines: u16,
    /// Other options will be read dynamically from config.
    config: config::Manager,
}

impl SessionSpool for Vt100Lines {
    fn resize(&mut self, size: TtySize) {
        self.parser.screen_mut().set_size(size.rows, self.config.vterm_width());
    }

    fn restore_buffer(&self) -> Vec<u8> {
        let (rows, cols) = self.parser.screen().size();
        info!("computing lines({}) restore buf with (rows={}, cols={})", self.nlines, rows, cols);
        self.parser.screen().last_n_rows_contents_formatted(self.nlines)
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes)
    }
}

/// A spool that restores the last screenful of content using shpool-vterm.
pub struct Vterm {
    term: shpool_vterm::Term,
    mode: SessionRestoreMode,
}

impl SessionSpool for Vterm {
    fn resize(&mut self, size: TtySize) {
        self.term
            .resize(shpool_vterm::Size { height: size.rows as usize, width: size.cols as usize });
    }

    fn restore_buffer(&self) -> Vec<u8> {
        match self.mode {
            SessionRestoreMode::Simple => vec![],
            SessionRestoreMode::Screen => self.term.contents(shpool_vterm::ContentRegion::Screen),
            SessionRestoreMode::Lines(nlines) => {
                self.term.contents(shpool_vterm::ContentRegion::BottomLines(nlines as usize))
            }
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.term.process(bytes);
    }
}

/// Creates a spool given a `mode`.
pub fn new(
    config: config::Manager,
    size: &TtySize,
    scrollback_lines: usize,
) -> Box<dyn SessionSpool + 'static> {
    let restore_engine = config.get().session_restore_engine.clone().unwrap_or_default();
    let mode = config.get().session_restore_mode.clone().unwrap_or_default();
    let vterm_width = config.vterm_width();
    match (mode, restore_engine) {
        (SessionRestoreMode::Simple, _) => Box::new(NullSpool),
        (SessionRestoreMode::Screen, SessionRestoreEngine::Vt100) => Box::new(Vt100Screen {
            parser: shpool_vt100::Parser::new(size.rows, vterm_width, scrollback_lines),
            config,
        }),
        (SessionRestoreMode::Lines(nlines), SessionRestoreEngine::Vt100) => Box::new(Vt100Lines {
            parser: shpool_vt100::Parser::new(size.rows, vterm_width, scrollback_lines),
            nlines,
            config,
        }),
        (mode, SessionRestoreEngine::Vterm) => Box::new(Vterm {
            term: shpool_vterm::Term::new(
                scrollback_lines,
                shpool_vterm::Size { width: size.cols as usize, height: size.rows as usize },
            ),
            mode,
        }),
    }
}

/// A spool that survives a panic in the terminal emulator.
///
/// shpool_vt100 assumes its cursor row is always valid and unwraps it; a
/// size or escape sequence that breaks that assumption used to panic the
/// shell->client thread, which never comes back: every later attach to the
/// session then fails until the session is killed. On a panic the spool is
/// rebuilt empty at the last size, so the session keeps working and only the
/// restore content of that moment is lost. Sizes with a zero dimension are
/// ignored here as well.
pub struct Guarded {
    inner: Box<dyn SessionSpool + 'static>,
    config: config::Manager,
    size: TtySize,
    scrollback_lines: usize,
}

impl Guarded {
    pub fn new(config: config::Manager, size: &TtySize, scrollback_lines: usize) -> Self {
        let size = if size.is_usable() { size.clone() } else { DEFAULT_TTY_SIZE };
        Guarded {
            inner: new(config.clone(), &size, scrollback_lines),
            config,
            size,
            scrollback_lines,
        }
    }

    fn guard<R>(&mut self, what: &str, f: impl FnOnce(&mut dyn SessionSpool) -> R) -> Option<R> {
        let inner = &mut self.inner;
        match panic::catch_unwind(AssertUnwindSafe(|| f(inner.as_mut()))) {
            Ok(r) => Some(r),
            Err(_) => {
                error!(
                    "session restore spool panicked during {} (rows={}, cols={}), resetting it",
                    what, self.size.rows, self.size.cols
                );
                self.inner = new(self.config.clone(), &self.size, self.scrollback_lines);
                None
            }
        }
    }

    pub fn resize(&mut self, size: TtySize) {
        if !size.is_usable() {
            error!("ignoring unusable spool size (rows={}, cols={})", size.rows, size.cols);
            return;
        }
        self.size = size.clone();
        self.guard("resize", |s| s.resize(size));
    }

    pub fn restore_buffer(&mut self) -> Vec<u8> {
        self.guard("restore", |s| s.restore_buffer()).unwrap_or_default()
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.guard("process", |s| s.process(bytes));
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn size(rows: u16, cols: u16) -> TtySize {
        TtySize { rows, cols, xpixel: 0, ypixel: 0 }
    }

    fn screen_config(dir: &tempfile::TempDir) -> anyhow::Result<config::Manager> {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "session_restore_mode = \"screen\"\n")?;
        config::Manager::new(path.to_str())
    }

    /// Stands in for a terminal emulator bug.
    struct PanickingSpool;
    impl SessionSpool for PanickingSpool {
        fn resize(&mut self, _: TtySize) {}
        fn restore_buffer(&self) -> Vec<u8> {
            panic!("restore bug")
        }
        fn process(&mut self, _: &[u8]) {
            panic!("process bug")
        }
    }

    #[test]
    fn guarded_spool_survives_a_panic() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = screen_config(&dir)?;
        let mut spool = Guarded::new(config, &size(24, 80), 100);
        spool.inner = Box::new(PanickingSpool);

        spool.process(b"lost\r\n");
        spool.process(b"still-here\r\n");
        let restored = String::from_utf8_lossy(&spool.restore_buffer()).to_string();
        assert!(restored.contains("still-here"), "restore after a reset: {restored:?}");

        spool.inner = Box::new(PanickingSpool);
        assert!(spool.restore_buffer().is_empty(), "a panicking restore yields nothing");
        spool.process(b"again\r\n");
        let restored = String::from_utf8_lossy(&spool.restore_buffer()).to_string();
        assert!(restored.contains("again"), "restore after a second reset: {restored:?}");
        Ok(())
    }

    #[test]
    fn guarded_spool_ignores_zero_sizes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = screen_config(&dir)?;
        let mut spool = Guarded::new(config, &size(0, 0), 100);
        spool.process(b"first\r\n");
        spool.resize(size(0, 80));
        spool.resize(size(24, 0));
        spool.process(b"second\r\n");
        let restored = String::from_utf8_lossy(&spool.restore_buffer()).to_string();
        assert!(restored.contains("first") && restored.contains("second"), "{restored:?}");
        assert_eq!(spool.size.rows, DEFAULT_TTY_SIZE.rows);
        Ok(())
    }
}
