use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use ch7800_protocol::{Command, Response};
use clap::{error::ErrorKind, CommandFactory, Parser};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};

use crate::{device::Device, Palette};

struct Screen {
    log: Vec<String>,
    last_notification: Option<(String, usize)>,
    input: Option<String>,
    completion: Option<Completion>,
    help: bool,
    help_scroll: u16,
    port: String,
    playback: &'static str,
    volume: Option<u16>,
    files: Option<u16>,
}

const HELP: &[&str] = &[
    "  Type : followed by a command. Tab completes; Enter sends.",
    "",
    "  PLAYBACK",
    "  track N                 Play file N (e.g. track 2 = 002.mp3)",
    "  next / previous         Move through tracks",
    "  play / pause / stop     Playback control",
    "  loop-track N            Repeat a chosen track",
    "  loop-current on|off     Repeat the playing track",
    "  loop-all / shuffle      All tracks in order / random",
    "  sequence N N ...        Play a sequence (up to 30 tracks)",
    "",
    "  VOLUME",
    "  volume N                Set level 0..30",
    "  volume N --remember     Save volume to the chip",
    "  volume-up / volume-down Step up or down (read/set/verify)",
    "  raw-volume-up           Send raw 0x14 (may be ignored)",
    "  raw-volume-down         Send raw 0x15 (may be ignored)",
    "",
    "  QUERIES",
    "  status                  Playing / paused / stopped",
    "  get-volume              Current volume",
    "  file-count              Number of stored audio files",
    "  version                 Firmware version",
    "",
    "  SETTINGS  (persistent actions require --confirm)",
    "  set-baud RATE --confirm Save UART baud rate",
    "  reset --confirm         Stop and reset the chip",
    "  factory-reset --confirm Erase saved chip settings",
    "",
    "  KEYS  ←/→ tracks  p play  Space pause  x stop",
    "        +/- volume   s status  v volume  f files",
    "        : command   ? help    q quit",
];

fn cli_error_summary(error: &clap::Error) -> String {
    let message = error.to_string();
    if error.kind() == ErrorKind::MissingRequiredArgument {
        if let Some(usage) = message
            .lines()
            .find_map(|line| line.trim().strip_prefix("Usage: "))
        {
            return format!(
                "Missing argument; use :{}",
                usage.strip_prefix("ch7800 ").unwrap_or(usage)
            );
        }
    }
    message
        .lines()
        .find(|line| line.trim_start().starts_with("error:"))
        .or_else(|| message.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or("Invalid command")
        .trim()
        .to_owned()
}

struct Completion {
    start: usize,
    options: Vec<String>,
    selected: Option<usize>,
}

impl Completion {
    fn tab(input: &mut String, state: &mut Option<Self>, reverse: bool) {
        if let Some(active) = state {
            let index = match active.selected {
                Some(index) if reverse => (index + active.options.len() - 1) % active.options.len(),
                Some(index) => (index + 1) % active.options.len(),
                None if reverse => active.options.len() - 1,
                None => 0,
            };
            input.truncate(active.start);
            input.push_str(&active.options[index]);
            active.selected = Some(index);
            return;
        }

        let start = input.rfind(' ').map_or(0, |index| index + 1);
        let prefix = &input[start..];
        let options = Self::candidates(&input[..start])
            .into_iter()
            .filter(|candidate| candidate.starts_with(prefix))
            .collect::<Vec<_>>();
        if options.is_empty() {
            return;
        }
        if options.len() == 1 {
            input.truncate(start);
            input.push_str(&options[0]);
            input.push(' ');
            return;
        }

        let common = options
            .iter()
            .skip(1)
            .fold(options[0].len(), |length, candidate| {
                options[0]
                    .bytes()
                    .zip(candidate.bytes())
                    .take(length)
                    .take_while(|(a, b)| a == b)
                    .count()
            });
        if common > prefix.len() {
            input.truncate(start);
            input.push_str(&options[0][..common]);
        }
        *state = Some(Self {
            start,
            options,
            selected: None,
        });
    }

    fn candidates(before: &str) -> Vec<String> {
        let words = before.split_whitespace().collect::<Vec<_>>();
        if words.is_empty() {
            return Palette::command()
                .get_subcommands()
                .map(|command| command.get_name())
                .filter(|name| *name != "ports" && *name != "tui" && *name != "help")
                .map(str::to_owned)
                .collect();
        }

        let command = words[0];
        let mut options = match (command, words.len()) {
            ("loop-current", 1) => vec!["on".into(), "off".into()],
            ("set-baud", 1) => [1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200]
                .iter()
                .map(u32::to_string)
                .collect(),
            _ => Vec::new(),
        };
        if let Some(subcommand) = Palette::command().find_subcommand(command) {
            options.extend(
                subcommand
                    .get_arguments()
                    .filter_map(|arg| arg.get_long())
                    .map(|flag| format!("--{flag}"))
                    .filter(|flag| !words.contains(&flag.as_str())),
            );
        }
        options
    }

    fn hint(&self, width: u16) -> Line<'_> {
        let mut spans = vec![Span::styled(
            " Tab ↹  ",
            Style::default().fg(Color::DarkGray),
        )];
        let mut used = 8;
        let first = self.selected.unwrap_or(0);
        for (index, option) in self.options.iter().enumerate().skip(first) {
            let needed = option.len() + 2;
            if used + needed > usize::from(width.saturating_sub(2)) {
                break;
            }
            spans.push(Span::styled(
                option.as_str(),
                if self.selected == Some(index) {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::Cyan)
                },
            ));
            spans.push(Span::raw("  "));
            used += needed;
        }
        Line::from(spans)
    }
}

impl Screen {
    fn push(&mut self, line: impl Into<String>) {
        self.last_notification = None;
        for part in line.into().lines() {
            self.log.push(part.to_owned());
        }
        if self.log.len() > 100 {
            self.log.drain(..self.log.len() - 100);
        }
    }

    fn push_notification(&mut self, message: String) {
        if let Some((previous, count)) = &mut self.last_notification {
            if *previous == message {
                *count += 1;
                if let Some(line) = self.log.last_mut() {
                    *line = format!("{message}  ×{count}");
                }
                return;
            }
        }
        self.push(message.clone());
        self.last_notification = Some((message, 1));
    }

    fn command(&mut self, device: &mut Device, cmd: Command<'_>) {
        self.push(format!("→ {cmd:?}"));
        match device.execute(&cmd) {
            Ok(responses) => {
                for response in responses {
                    self.observe(&response);
                }
                let follow_up = if cmd.starts_audio() {
                    Some(Command::Status)
                } else if matches!(
                    cmd,
                    Command::Volume { .. } | Command::VolumeUp | Command::VolumeDown
                ) {
                    Some(Command::GetVolume)
                } else {
                    None
                };
                if let Some(query) = follow_up {
                    match device.execute(&query) {
                        Ok(responses) => {
                            for response in responses {
                                self.observe(&response);
                            }
                        }
                        Err(error) => self.push(format!("! status refresh: {error:#}")),
                    }
                }
            }
            Err(error) => self.push(format!("! {error:#}")),
        }
    }

    fn adjust_volume(&mut self, device: &mut Device, delta: i8) {
        self.push(if delta > 0 {
            "→ + volume"
        } else {
            "→ - volume"
        });
        match device.adjust_volume(delta) {
            Ok(responses) => {
                for response in responses {
                    self.observe(&response);
                }
            }
            Err(error) => self.push(format!("! {error:#}")),
        }
    }

    fn observe(&mut self, response: &Response) {
        if let Response::Frame { code, .. } = response {
            let data = response.payload();
            if data.len() == 2 {
                let value = u16::from_be_bytes([data[0], data[1]]);
                match code {
                    0x42 => {
                        self.playback = match data[1] {
                            1 => "playing",
                            2 => "paused",
                            0 => "stopped",
                            _ => "unknown",
                        }
                    }
                    0x43 => self.volume = Some(value),
                    0x4a => self.files = Some(value),
                    0x3d | 0x3e => self.playback = "stopped",
                    _ => {}
                }
            }
        }
        if !matches!(response, Response::Ack) {
            if matches!(response, Response::Frame { code: 0x30, .. }) {
                self.push_notification(response.to_string());
            } else {
                self.push(response.to_string());
            }
        }
    }
}

struct RawTerminal(Terminal<CrosstermBackend<Stdout>>);

impl RawTerminal {
    fn start() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            disable_raw_mode()?;
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => Ok(Self(terminal)),
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                Err(error.into())
            }
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = self.0.show_cursor();
    }
}

pub fn run(mut device: Device, path: &str) -> Result<()> {
    let mut terminal = RawTerminal::start()?;
    let mut screen = Screen {
        log: vec!["Connected. No playback starts automatically.".into()],
        last_notification: None,
        input: None,
        completion: None,
        help: false,
        help_scroll: 0,
        port: path.into(),
        playback: "unknown",
        volume: None,
        files: None,
    };
    for command in [Command::Status, Command::GetVolume, Command::FileCount] {
        screen.command(&mut device, command);
    }
    loop {
        terminal.0.draw(|frame| {
            let chunks = Layout::default().direction(Direction::Vertical)
                .constraints([Constraint::Length(4), Constraint::Length(4), Constraint::Min(5), Constraint::Length(4)])
                .split(frame.area());
            frame.render_widget(Paragraph::new(vec![Line::from(vec![
                Span::styled(" CH7800 ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(format!("  {}", screen.port)),
            ]), Line::raw(format!("  {:<10}  volume {:>2}/30  •  {} files",
                screen.playback,
                screen.volume.map_or("--".into(), |v| v.to_string()),
                screen.files.map_or("--".into(), |n| n.to_string())))]).block(Block::default().borders(Borders::ALL)), chunks[0]);
            frame.render_widget(Paragraph::new("←/→ previous/next   p play   space pause   x stop   +/- volume\ns status   v volume   f files   : commands   q quit")
                .block(Block::default().title("Keys").borders(Borders::ALL)), chunks[1]);
            if screen.help {
                let lines: Vec<Line> = HELP.iter().map(|text| {
                    let style = if ["PLAYBACK", "VOLUME", "QUERIES", "SETTINGS", "KEYS"]
                        .iter().any(|heading| text.trim_start().starts_with(heading)) {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    Line::styled(*text, style)
                }).collect();
                frame.render_widget(Paragraph::new(lines)
                    .block(Block::default().title("Commands  •  ↑/↓ scroll  •  Esc close").borders(Borders::ALL))
                    .scroll((screen.help_scroll, 0)), chunks[2]);
                frame.render_widget(Paragraph::new(" : enter command    ↑/↓ scroll    Esc or ? close")
                    .block(Block::default().title("Help").borders(Borders::ALL)), chunks[3]);
            } else {
                let height = usize::from(chunks[2].height.saturating_sub(2));
                let lines: Vec<Line> = screen.log.iter().rev().take(height).rev().map(|s| Line::raw(s.as_str())).collect();
                frame.render_widget(Paragraph::new(lines).block(Block::default().title("UART activity").borders(Borders::ALL)).wrap(Wrap { trim: false }), chunks[2]);
                let prompt = screen.input.as_ref().map(|s| format!(":{s}█"))
                    .unwrap_or_else(|| "Press : for commands (e.g. track 1). Tab completes; Esc cancels.".into());
                let mut lines = vec![Line::raw(prompt)];
                if let Some(completion) = &screen.completion {
                    lines.push(completion.hint(chunks[3].width));
                }
                frame.render_widget(Paragraph::new(lines).block(Block::default().title("Command").borders(Borders::ALL)), chunks[3]);
            }
        })?;

        for message in device.poll()? {
            // Replies and asynchronous notifications use the same parser.
            screen.observe(&message);
        }
        if !event::poll(Duration::from_millis(40))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            break;
        }
        if screen.help {
            let visible = terminal.0.size()?.height.saturating_sub(14);
            let max_scroll = HELP.len().saturating_sub(usize::from(visible)) as u16;
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => screen.help = false,
                KeyCode::Char(':') => {
                    screen.help = false;
                    screen.input = Some(String::new());
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    screen.help_scroll = screen.help_scroll.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    screen.help_scroll = screen.help_scroll.saturating_add(1).min(max_scroll)
                }
                KeyCode::PageUp => {
                    screen.help_scroll = screen.help_scroll.saturating_sub(visible.max(1))
                }
                KeyCode::PageDown => {
                    screen.help_scroll = screen
                        .help_scroll
                        .saturating_add(visible.max(1))
                        .min(max_scroll)
                }
                _ => {}
            }
            continue;
        }
        if let Some(input) = screen.input.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    screen.input = None;
                    screen.completion = None;
                }
                KeyCode::Backspace => {
                    input.pop();
                    screen.completion = None;
                }
                KeyCode::Enter => {
                    let text = screen.input.take().unwrap();
                    screen.completion = None;
                    if text.trim().is_empty() || matches!(text.trim(), "help" | "?" | "--help") {
                        screen.help = true;
                        screen.help_scroll = 0;
                        continue;
                    }
                    // Only whitespace-separated numeric/flag arguments are needed by these commands.
                    let words: Vec<&str> = text.split_whitespace().collect();
                    match Palette::try_parse_from(std::iter::once("ch7800").chain(words)) {
                        Ok(palette) => match &palette.action {
                            crate::Action::VolumeUp => screen.adjust_volume(&mut device, 1),
                            crate::Action::VolumeDown => screen.adjust_volume(&mut device, -1),
                            _ => match palette.action.command() {
                                Ok(cmd) => screen.command(&mut device, cmd),
                                Err(error) => screen.push(format!("! {error:#}")),
                            },
                        },
                        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
                            screen.help = true;
                            screen.help_scroll = 0;
                        }
                        Err(error) => screen.push(format!(
                            "! {}  (press ? for commands)",
                            cli_error_summary(&error)
                        )),
                    }
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    Completion::tab(
                        input,
                        &mut screen.completion,
                        key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                    );
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    screen.completion = None;
                }
                _ => {}
            }
            continue;
        }
        let cmd = match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char(':') => {
                screen.input = Some(String::new());
                screen.completion = None;
                continue;
            }
            KeyCode::Char('?') => {
                screen.help = true;
                screen.help_scroll = 0;
                continue;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                screen.adjust_volume(&mut device, 1);
                continue;
            }
            KeyCode::Char('-') => {
                screen.adjust_volume(&mut device, -1);
                continue;
            }
            KeyCode::Left => Some(Command::Previous),
            KeyCode::Right => Some(Command::Next),
            KeyCode::Char('p') => Some(Command::Play),
            KeyCode::Char(' ') => Some(Command::Pause),
            KeyCode::Char('x') => Some(Command::Stop),
            KeyCode::Char('s') => Some(Command::Status),
            KeyCode::Char('v') => Some(Command::GetVolume),
            KeyCode::Char('f') => Some(Command::FileCount),
            _ => None,
        };
        if let Some(cmd) = cmd {
            screen.command(&mut device, cmd);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{cli_error_summary, Completion, Palette, Screen, HELP};
    use clap::{error::ErrorKind, Parser};

    #[test]
    fn completes_command_and_arguments() {
        let mut state = None;
        let mut input = "tra".to_string();
        Completion::tab(&mut input, &mut state, false);
        assert_eq!(input, "track ");
        assert!(state.is_none());

        input = "loop-current on".into();
        Completion::tab(&mut input, &mut state, false);
        assert_eq!(input, "loop-current on ");

        input = "volume 7 --rem".into();
        Completion::tab(&mut input, &mut state, false);
        assert_eq!(input, "volume 7 --remember ");

        input = "set-baud 96".into();
        Completion::tab(&mut input, &mut state, false);
        assert_eq!(input, "set-baud 9600 ");
    }

    #[test]
    fn ambiguous_matches_cycle_in_both_directions() {
        let mut state = None;
        let mut input = "vol".to_string();
        Completion::tab(&mut input, &mut state, false);
        assert_eq!(input, "volume");
        assert!(state.is_some());

        Completion::tab(&mut input, &mut state, false);
        assert_eq!(input, "volume-up");
        Completion::tab(&mut input, &mut state, true);
        assert_eq!(input, "volume");
    }

    #[test]
    fn no_match_preserves_input() {
        let mut state = None;
        let mut input = "unknown".to_string();
        Completion::tab(&mut input, &mut state, false);
        assert_eq!(input, "unknown");
        assert!(state.is_none());
    }

    #[test]
    fn clap_help_is_not_an_activity_log_error() {
        let help = Palette::try_parse_from(["ch7800", "help"]).err().unwrap();
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        assert!(HELP.iter().any(|line| line.contains("track N")));

        let invalid = Palette::try_parse_from(["ch7800", "invalid-command"])
            .err()
            .unwrap();
        let summary = cli_error_summary(&invalid);
        assert!(summary.contains("invalid-command"));
        assert!(!summary.contains('\n'));

        let missing = Palette::try_parse_from(["ch7800", "loop-track"])
            .err()
            .unwrap();
        assert_eq!(
            cli_error_summary(&missing),
            "Missing argument; use :loop-track <NUMBER>"
        );
    }

    #[test]
    fn log_keeps_lines_separate() {
        let mut screen = Screen {
            log: Vec::new(),
            last_notification: None,
            input: None,
            completion: None,
            help: false,
            help_scroll: 0,
            port: String::new(),
            playback: "unknown",
            volume: None,
            files: None,
        };
        screen.push("first\nsecond");
        assert_eq!(screen.log, ["first", "second"]);
    }

    #[test]
    fn coalesces_repeated_undocumented_notifications() {
        let mut screen = Screen {
            log: Vec::new(),
            last_notification: None,
            input: None,
            completion: None,
            help: false,
            help_scroll: 0,
            port: String::new(),
            playback: "unknown",
            volume: None,
            files: None,
        };
        let mut data = [0; 30];
        data[1] = 2;
        let response = super::Response::Frame {
            code: 0x30,
            data,
            len: 2,
        };
        for _ in 0..8 {
            screen.observe(&response);
        }
        assert_eq!(screen.log.len(), 1);
        assert!(screen.log[0].contains("×8"));

        screen.push("→ Status");
        screen.observe(&response);
        assert_eq!(screen.log.len(), 3);
        assert!(!screen.log[2].contains('×'));
    }
}
