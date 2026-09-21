//! Colour, glyphs and motion for what a person reads (DESIGN §5.2).
//!
//! **The palette is defined here and nowhere else.** Deep Sea blues are the
//! base; one bright accent marks fork and success; amber marks discard and
//! warnings. The two darkest blues are backgrounds only — for badges and bars
//! with light text on them — because as text they vanish on a dark terminal.
//!
//! **When there is no colour or motion at all:** under `memfork mcp` (its
//! stdout is a protocol), under `--json` (its stdout is data), and — unless
//! `--color always` says otherwise — when stdout is not a terminal, in CI, when
//! `NO_COLOR` is set, or when the terminal says it is dumb. `--color always`
//! beats the environment, because an explicit flag is the more specific
//! instruction; it never beats `--json` or `mcp`, which are not about taste.
//!
//! **Colour is never the only signal.** Every state printed in colour also
//! has a word, and every glyph has an ASCII stand-in, so output survives a
//! monochrome terminal, a screen reader and a log file intact. No emoji.

use std::io::IsTerminal;

/// A colour, as 24-bit RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// The palette. Change a colour here and it changes everywhere.
pub mod palette {
    use super::Rgb;

    /// Primary text: names, values, the things being read.
    pub const PRIMARY: Rgb = Rgb(0x9E, 0xB3, 0xC2);
    /// Secondary and dim text: ids, sequence numbers, connecting lines.
    pub const SECONDARY: Rgb = Rgb(0x1C, 0x72, 0x93);
    /// Badge and bar background. Never text: unreadable on a dark terminal.
    pub const DEEP: Rgb = Rgb(0x06, 0x5A, 0x82);
    /// Badge and bar background. Never text, for the same reason.
    pub const NAVY: Rgb = Rgb(0x21, 0x29, 0x5C);
    /// Text on [`DEEP`] and [`NAVY`].
    pub const ON_BADGE: Rgb = Rgb(0xF2, 0xF6, 0xF9);
    /// The one bright accent: forks, merges that landed, success.
    pub const ACCENT: Rgb = Rgb(0x3D, 0xDC, 0x97);
    /// Discards and warnings.
    pub const AMBER: Rgb = Rgb(0xE8, 0xA3, 0x3D);
}

/// `--color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
    /// Colour when the rules below allow it.
    #[default]
    Auto,
    /// Colour even when stdout is not a terminal, in CI, or with `NO_COLOR`.
    Always,
    /// Never.
    Never,
}

impl ColorChoice {
    /// Parse a `--color` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(ColorChoice::Auto),
            "always" => Some(ColorChoice::Always),
            "never" => Some(ColorChoice::Never),
            _ => None,
        }
    }
}

/// What the rules look at, gathered once so they can be tested without a
/// terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Surroundings {
    /// Whether stdout is a terminal.
    pub stdout_tty: bool,
    /// Whether stderr is a terminal.
    pub stderr_tty: bool,
    /// Whether `CI` is set to something other than `false` or `0`.
    pub ci: bool,
    /// Whether `NO_COLOR` is set and not empty.
    pub no_color: bool,
    /// Whether `TERM` is `dumb`.
    pub dumb: bool,
    /// Whether the terminal can be trusted with box-drawing characters.
    pub unicode: bool,
}

impl Surroundings {
    /// Look at this process's terminal and environment.
    pub fn here() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let stdout_tty = std::io::stdout().is_terminal();
        let dumb = var("TERM").is_some_and(|t| t == "dumb");
        Surroundings {
            stdout_tty,
            stderr_tty: std::io::stderr().is_terminal(),
            ci: var("CI").is_some_and(|v| v != "false" && v != "0"),
            no_color: var("NO_COLOR").is_some(),
            dumb,
            unicode: stdout_tty && !dumb && unicode_terminal(&var),
        }
    }
}

/// Whether the terminal renders box-drawing characters: a UTF-8 locale on
/// Unix; on Windows, a terminal known to (Windows Terminal, VS Code, ConEmu)
/// rather than the legacy console, whose default code page does not.
fn unicode_terminal(var: &dyn Fn(&str) -> Option<String>) -> bool {
    if cfg!(windows) {
        var("WT_SESSION").is_some() || var("TERM_PROGRAM").is_some() || var("ConEmuANSI").is_some()
    } else {
        ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|name| var(name))
            .is_some_and(|v| {
                let v = v.to_ascii_lowercase();
                v.contains("utf-8") || v.contains("utf8")
            })
    }
}

/// Where output is going, which settles some questions outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Text for a person.
    Human,
    /// `--json`: data, never decorated.
    Json,
    /// `memfork mcp`: a protocol stream, never decorated.
    Protocol,
}

/// Whether to colour stdout.
pub fn colour(choice: ColorChoice, channel: Channel, env: &Surroundings) -> bool {
    if channel != Channel::Human {
        return false;
    }
    match choice {
        ColorChoice::Never => false,
        ColorChoice::Always => true,
        ColorChoice::Auto => env.stdout_tty && !env.ci && !env.no_color && !env.dumb,
    }
}

/// Whether to animate on stderr: a spinner is only ever drawn on a terminal,
/// and only where colour would be allowed, so that CI logs, pipes and
/// `NO_COLOR` get plain lines — or, with `--color always`, colour but still no
/// animation if stderr is not a terminal.
pub fn motion(choice: ColorChoice, channel: Channel, env: &Surroundings) -> bool {
    if channel != Channel::Human || !env.stderr_tty || env.dumb {
        return false;
    }
    match choice {
        ColorChoice::Never => false,
        ColorChoice::Always => true,
        ColorChoice::Auto => env.stdout_tty && !env.ci && !env.no_color,
    }
}

/// Glyphs, each with an ASCII stand-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// A commit.
    Commit,
    /// A lane continuing.
    Lane,
    /// A lane joining from the right.
    JoinRight,
    /// A lane splitting off to the right.
    SplitRight,
    /// A horizontal run between lanes.
    Across,
    /// A lane passing a junction.
    Tee,
    /// A junction crossing a lane it does not join.
    Cross,
    /// A discarded branch.
    Discarded,
    /// Connected, running.
    Connected,
    /// Not connected.
    Waiting,
    /// A key added.
    Added,
    /// A key removed.
    Removed,
    /// A key changed.
    Modified,
}

impl Glyph {
    fn text(self, unicode: bool) -> &'static str {
        match (self, unicode) {
            (Glyph::Commit, true) => "●",
            (Glyph::Commit, false) => "*",
            (Glyph::Lane, true) => "│",
            (Glyph::Lane, false) => "|",
            (Glyph::JoinRight, true) => "╯",
            (Glyph::JoinRight, false) => "/",
            (Glyph::SplitRight, true) => "╮",
            (Glyph::SplitRight, false) => "\\",
            (Glyph::Across, true) => "─",
            (Glyph::Across, false) => "-",
            (Glyph::Tee, true) => "├",
            (Glyph::Tee, false) => "|",
            (Glyph::Cross, true) => "┼",
            (Glyph::Cross, false) => "+",
            (Glyph::Discarded, true) => "×",
            (Glyph::Discarded, false) => "x",
            (Glyph::Connected, true) => "●",
            (Glyph::Connected, false) => "*",
            (Glyph::Waiting, true) => "○",
            (Glyph::Waiting, false) => "o",
            (Glyph::Added, _) => "+",
            (Glyph::Removed, _) => "-",
            (Glyph::Modified, _) => "~",
        }
    }
}

/// How to decorate output: whether to colour, and which glyphs to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// Emit colour escapes.
    pub colour: bool,
    /// Use box-drawing glyphs rather than ASCII.
    pub unicode: bool,
}

impl Style {
    /// No colour, ASCII glyphs: what a log file or a test sees.
    pub const PLAIN: Style = Style {
        colour: false,
        unicode: false,
    };

    /// The style for this process's stdout.
    pub fn for_stdout(choice: ColorChoice, channel: Channel) -> Self {
        let env = Surroundings::here();
        let colour = colour(choice, channel, &env);
        if colour {
            // A Windows console interprets escapes only once asked to. A
            // no-op everywhere else.
            let _ = anstyle_query::windows::enable_ansi_colors();
        }
        Style {
            colour,
            unicode: channel == Channel::Human && env.unicode,
        }
    }

    /// A glyph in this style.
    pub fn glyph(&self, g: Glyph) -> &'static str {
        g.text(self.unicode)
    }

    /// `text` in a foreground colour.
    pub fn fg(&self, colour: Rgb, text: &str) -> String {
        if !self.colour {
            return text.to_owned();
        }
        let Rgb(r, g, b) = colour;
        format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
    }

    /// `text` in bold in a foreground colour.
    pub fn strong(&self, colour: Rgb, text: &str) -> String {
        if !self.colour {
            return text.to_owned();
        }
        let Rgb(r, g, b) = colour;
        format!("\x1b[1;38;2;{r};{g};{b}m{text}\x1b[0m")
    }

    /// `text` as a badge: light text on one of the deep backgrounds, padded.
    /// Without colour it is the bare word in brackets, so it still reads as a
    /// label.
    pub fn badge(&self, background: Rgb, text: &str) -> String {
        if !self.colour {
            return format!("[{text}]");
        }
        let Rgb(r, g, b) = background;
        let Rgb(fr, fg, fb) = palette::ON_BADGE;
        format!("\x1b[38;2;{fr};{fg};{fb};48;2;{r};{g};{b}m {text} \x1b[0m")
    }

    /// Primary text.
    pub fn primary(&self, text: &str) -> String {
        self.fg(palette::PRIMARY, text)
    }

    /// Secondary, dimmer text.
    pub fn dim(&self, text: &str) -> String {
        self.fg(palette::SECONDARY, text)
    }

    /// The accent: forks and success.
    pub fn accent(&self, text: &str) -> String {
        self.fg(palette::ACCENT, text)
    }

    /// Discards and warnings.
    pub fn warn(&self, text: &str) -> String {
        self.fg(palette::AMBER, text)
    }
}

/// What this process was asked for, recorded once at start-up so that code
/// deep inside — the registry asking a client's command, say — can decide
/// whether to draw a spinner without every caller passing it down.
static PREFERENCES: std::sync::OnceLock<(ColorChoice, Channel)> = std::sync::OnceLock::new();

/// Record this process's `--color` and output channel. The first call wins.
pub fn set_preferences(choice: ColorChoice, channel: Channel) {
    let _ = PREFERENCES.set((choice, channel));
}

/// This process's `--color` and output channel. A library caller that never
/// set them gets the protocol channel: no decoration at all.
pub fn preferences() -> (ColorChoice, Channel) {
    PREFERENCES
        .get()
        .copied()
        .unwrap_or((ColorChoice::Never, Channel::Protocol))
}

/// A spinner on stderr, for something that really takes a while: starting the
/// daemon (which replays the log), asking a client's own command.
///
/// Drawn only when [`motion`] allows. Otherwise the message is printed once,
/// as a plain line, if the caller wants a log to say what was waited for.
#[derive(Debug)]
pub struct Spinner {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    /// Start one. `message` says what is being waited for.
    pub fn start(choice: ColorChoice, channel: Channel, message: &str) -> Spinner {
        Spinner::begin(choice, channel, message, true)
    }

    /// A spinner with this process's preferences that prints nothing at all
    /// when it cannot animate: for short waits a log has no need to mention.
    pub fn quiet(message: &str) -> Spinner {
        let (choice, channel) = preferences();
        Spinner::begin(choice, channel, message, false)
    }

    fn begin(choice: ColorChoice, channel: Channel, message: &str, say_it: bool) -> Spinner {
        let env = Surroundings::here();
        let none = Spinner {
            stop: None,
            thread: None,
        };
        if channel == Channel::Protocol {
            return none;
        }
        if !motion(choice, channel, &env) {
            if say_it {
                eprintln!("memfork: {message}");
            }
            return none;
        }
        let frames: &[&str] = if env.unicode {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        } else {
            &["|", "/", "-", "\\"]
        };
        let style = Style {
            colour: colour(choice, channel, &env),
            unicode: env.unicode,
        };
        let message = message.to_owned();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            use std::io::Write as _;
            let mut i = 0usize;
            loop {
                let frame = style.accent(frames[i % frames.len()]);
                let mut err = std::io::stderr().lock();
                let _ = write!(err, "\r{frame} {message}");
                let _ = err.flush();
                drop(err);
                i += 1;
                match rx.recv_timeout(std::time::Duration::from_millis(90)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    _ => break,
                }
            }
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        });
        Spinner {
            stop: Some(tx),
            thread: Some(thread),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> Surroundings {
        Surroundings {
            stdout_tty: true,
            stderr_tty: true,
            unicode: true,
            ..Surroundings::default()
        }
    }

    #[test]
    fn auto_colours_a_plain_terminal_and_nothing_else() {
        let t = terminal();
        assert!(colour(ColorChoice::Auto, Channel::Human, &t));
        for env in [
            Surroundings {
                stdout_tty: false,
                ..t.clone()
            },
            Surroundings {
                ci: true,
                ..t.clone()
            },
            Surroundings {
                no_color: true,
                ..t.clone()
            },
            Surroundings {
                dumb: true,
                ..t.clone()
            },
        ] {
            assert!(!colour(ColorChoice::Auto, Channel::Human, &env), "{env:?}");
        }
    }

    #[test]
    fn always_beats_the_environment_but_not_json_or_the_protocol() {
        let hostile = Surroundings {
            stdout_tty: false,
            stderr_tty: false,
            ci: true,
            no_color: true,
            dumb: false,
            unicode: false,
        };
        assert!(colour(ColorChoice::Always, Channel::Human, &hostile));
        assert!(!colour(ColorChoice::Always, Channel::Json, &terminal()));
        assert!(!colour(ColorChoice::Always, Channel::Protocol, &terminal()));
        assert!(!colour(ColorChoice::Never, Channel::Human, &terminal()));
    }

    #[test]
    fn motion_needs_a_terminal_on_stderr_whatever_the_flag() {
        let t = terminal();
        assert!(motion(ColorChoice::Auto, Channel::Human, &t));
        let piped = Surroundings {
            stderr_tty: false,
            ..t.clone()
        };
        assert!(!motion(ColorChoice::Always, Channel::Human, &piped));
        assert!(!motion(
            ColorChoice::Auto,
            Channel::Human,
            &Surroundings {
                ci: true,
                ..t.clone()
            }
        ));
        assert!(!motion(
            ColorChoice::Auto,
            Channel::Human,
            &Surroundings {
                no_color: true,
                ..t.clone()
            }
        ));
        assert!(!motion(ColorChoice::Always, Channel::Json, &t));
        assert!(!motion(ColorChoice::Always, Channel::Protocol, &t));
    }

    #[test]
    fn plain_output_carries_words_and_ascii_only() {
        let s = Style::PLAIN;
        assert_eq!(s.accent("forked"), "forked");
        assert_eq!(s.badge(palette::DEEP, "main"), "[main]");
        for g in [
            Glyph::Commit,
            Glyph::Lane,
            Glyph::JoinRight,
            Glyph::SplitRight,
            Glyph::Across,
            Glyph::Tee,
            Glyph::Cross,
            Glyph::Discarded,
            Glyph::Connected,
            Glyph::Waiting,
        ] {
            assert!(s.glyph(g).is_ascii(), "{g:?}");
        }
    }

    #[test]
    fn coloured_output_is_24_bit_and_reset() {
        let s = Style {
            colour: true,
            unicode: true,
        };
        let text = s.accent("ok");
        assert!(text.starts_with("\x1b[38;2;61;220;151m"), "{text:?}");
        assert!(text.ends_with("\x1b[0m"));
        let badge = s.badge(palette::NAVY, "main");
        assert!(badge.contains("48;2;33;41;92"), "{badge:?}");
    }

    #[test]
    fn the_dark_blues_are_only_ever_backgrounds() {
        // The rule, enforced across the crate: DEEP and NAVY reach the
        // terminal only through `badge`. The patterns are assembled here so
        // this test cannot match itself.
        let mut bad = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                for call in ["fg", "strong"] {
                    for name in ["DEEP", "NAVY"] {
                        let pattern = format!("{call}(palette::{name}");
                        if text.contains(&pattern) {
                            bad.push(format!("{}: {pattern}", path.display()));
                        }
                    }
                }
            }
        }
        assert!(bad.is_empty(), "dark blues used as text: {bad:?}");
    }
}
