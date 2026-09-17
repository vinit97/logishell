//! Small alternate-screen menus for setup, with no persistent terminal changes.

use anyhow::{Context, Result, bail, ensure};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event as InputEvent, EventStream, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size,
    },
};
use futures_util::StreamExt;
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc, Once,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use unicode_width::UnicodeWidthStr;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();
const MAX_INPUT: usize = 4096;

#[derive(Clone)]
pub struct Choice {
    pub label: String,
    pub detail: String,
}

pub enum Event {
    Status {
        title: String,
        message: String,
    },
    Choose {
        title: String,
        message: String,
        choices: Vec<Choice>,
        reply: oneshot::Sender<Option<usize>>,
    },
    Input {
        title: String,
        message: String,
        reply: oneshot::Sender<Option<String>>,
    },
    Dismiss,
}

#[derive(Clone)]
pub struct PairingUi {
    events: mpsc::UnboundedSender<Event>,
    cancel: Arc<AtomicBool>,
    canceled: watch::Sender<bool>,
}

pub fn channel() -> (PairingUi, mpsc::UnboundedReceiver<Event>) {
    let (events, receiver) = mpsc::unbounded_channel();
    (
        PairingUi {
            events,
            cancel: Arc::new(AtomicBool::new(false)),
            canceled: watch::channel(false).0,
        },
        receiver,
    )
}

impl PairingUi {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.canceled.send_replace(true);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub(crate) fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub async fn cancelled_signal(&self) {
        let mut canceled = self.canceled.subscribe();
        if !*canceled.borrow_and_update() {
            let _ = canceled.changed().await;
        }
    }

    fn send(&self, event: Event) -> Result<()> {
        self.events
            .send(event)
            .map_err(|_| anyhow::anyhow!("pairing screen closed"))
    }

    pub fn status(&self, title: &str, message: &str) -> Result<()> {
        self.send(Event::Status {
            title: title.into(),
            message: message.into(),
        })
    }

    pub fn dismiss(&self) -> Result<()> {
        self.send(Event::Dismiss)
    }

    fn choice_request(
        &self,
        title: &str,
        message: &str,
        choices: Vec<Choice>,
    ) -> Result<oneshot::Receiver<Option<usize>>> {
        ensure!(!self.cancelled(), "pairing cancelled");
        ensure!(!choices.is_empty(), "no pairing choices available");
        let (reply, response) = oneshot::channel();
        self.send(Event::Choose {
            title: title.into(),
            message: message.into(),
            choices,
            reply,
        })?;
        Ok(response)
    }

    pub async fn choose(
        &self,
        title: &str,
        message: &str,
        choices: Vec<Choice>,
    ) -> Result<Option<usize>> {
        self.response(self.choice_request(title, message, choices)?)
            .await
    }

    pub fn choose_blocking(
        &self,
        title: &str,
        message: &str,
        choices: Vec<Choice>,
        timeout: Duration,
    ) -> Result<Option<usize>> {
        let mut response = self.choice_request(title, message, choices)?;
        let deadline = Instant::now() + timeout;
        loop {
            if self.cancelled() {
                return Ok(None);
            }
            match response.try_recv() {
                Ok(answer) => return Ok(answer),
                Err(oneshot::error::TryRecvError::Closed) => bail!("pairing selection dismissed"),
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
            ensure!(Instant::now() < deadline, "device selection timed out");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub async fn input(&self, title: &str, message: &str) -> Result<Option<String>> {
        ensure!(!self.cancelled(), "pairing cancelled");
        let (reply, response) = oneshot::channel();
        self.send(Event::Input {
            title: title.into(),
            message: message.into(),
            reply,
        })?;
        self.response(response).await
    }

    async fn response<T>(&self, response: oneshot::Receiver<Option<T>>) -> Result<Option<T>> {
        tokio::select! {
            response = response => response.context("pairing prompt dismissed"),
            _ = self.cancelled_signal() => Ok(None),
        }
    }
}

pub struct Terminal {
    events: EventStream,
    interrupt: Signal,
    terminate: Signal,
}

fn restore_terminal() -> io::Result<()> {
    // Always restore cooked input even when writing to stdout has failed.
    let raw = disable_raw_mode();
    let screen = execute!(
        io::stdout(),
        SetAttribute(Attribute::Reset),
        ResetColor,
        Show,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    raw.and(screen)
}

impl Terminal {
    /// Must run inside the Tokio runtime used by setup.
    pub fn enter() -> Result<Self> {
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "setup needs an interactive terminal"
        );
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if ACTIVE.swap(false, Ordering::SeqCst) {
                    let _ = restore_terminal();
                }
                previous(info);
            }));
        });
        let interrupt = signal(SignalKind::interrupt()).context("watching terminal interrupts")?;
        let terminate = signal(SignalKind::terminate()).context("watching terminal termination")?;
        ensure!(
            ACTIVE
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "another setup screen is already active"
        );
        // This guard restores the terminal even when opening the screen fails.
        let terminal = Self {
            events: EventStream::new(),
            interrupt,
            terminate,
        };
        enable_raw_mode().context("enabling terminal input")?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste
        )
        .context("opening setup screen")?;
        Ok(terminal)
    }

    pub async fn select(
        &mut self,
        title: &str,
        description: &str,
        choices: &[Choice],
        initial: usize,
    ) -> Result<Option<usize>> {
        ensure!(!choices.is_empty(), "this menu has no choices");
        let mut selected = initial.min(choices.len() - 1);
        let mut first = 0;
        loop {
            let (cols, rows) = size()?;
            let frame = menu_frame(
                cols,
                rows,
                title,
                description,
                choices,
                selected,
                &mut first,
            );
            if let InputEvent::Key(key) = self.read_frame(&frame).await? {
                match menu_key(key, selected, choices.len(), frame.capacity) {
                    Action::Select => return Ok(Some(selected)),
                    Action::Back => return Ok(None),
                    Action::Move(next) => selected = next,
                    _ => {}
                }
            }
        }
    }

    /// Append the same Back action to any menu, translating it to cancellation.
    pub async fn select_back(
        &mut self,
        title: &str,
        description: &str,
        choices: &[Choice],
        initial: usize,
    ) -> Result<Option<usize>> {
        let mut options = choices.to_vec();
        options.push(Choice {
            label: "Back".into(),
            detail: String::new(),
        });
        Ok(self
            .select(title, description, &options, initial)
            .await?
            .filter(|index| *index < choices.len()))
    }

    pub async fn input(
        &mut self,
        title: &str,
        description: &str,
        initial: &str,
    ) -> Result<Option<String>> {
        let mut editor = Editor::new(initial);
        loop {
            let (cols, rows) = size()?;
            match self
                .read_frame(&input_frame(cols, rows, title, description, &editor))
                .await?
            {
                InputEvent::Paste(text) => editor.insert(&text),
                InputEvent::Key(key) => match common_key(key) {
                    Action::Select => return Ok(Some(editor.text())),
                    Action::Back => return Ok(None),
                    _ => {
                        editor.key(key);
                    }
                },
                _ => {}
            }
        }
    }

    /// Paint a progress/result message without waiting for a key.
    pub fn message(&mut self, title: &str, message: &str) -> Result<()> {
        let (cols, rows) = size().context("reading terminal size")?;
        let mut frame = Frame::new(cols, rows, title, "", "Ctrl+C quit");
        for (offset, line) in wrap(message, frame.width)
            .into_iter()
            .take(frame.capacity)
            .enumerate()
        {
            frame.line(frame.start + offset as u16, line, Tone::Normal);
        }
        self.draw(&frame)
    }

    /// Keep authentication instructions visible while the pairing worker runs.
    pub async fn pairing_wait(&mut self, title: &str, message: &str) -> Result<()> {
        self.scroll_text(title, message, "Back (cancel pairing)", "Esc cancel")
            .await
    }

    pub async fn details(&mut self, title: &str, message: &str) -> Result<()> {
        self.scroll_text(title, message, "Back", "↑↓ scroll   Esc back")
            .await
    }

    async fn scroll_text(
        &mut self,
        title: &str,
        message: &str,
        back: &str,
        hint: &str,
    ) -> Result<()> {
        let mut first = 0;
        loop {
            let (cols, rows) = size()?;
            let mut frame = Frame::new(cols, rows, title, "", hint);
            let lines = wrap(message, frame.width);
            let capacity = frame.capacity.saturating_sub(1).max(1);
            first = first.min(lines.len().saturating_sub(capacity));
            for (offset, line) in lines.iter().skip(first).take(capacity).enumerate() {
                frame.line(frame.start + offset as u16, line.clone(), Tone::Normal);
            }
            if frame.capacity > 1 {
                frame.line(
                    frame.start + frame.capacity as u16 - 1,
                    format!("› {back}"),
                    Tone::Selected,
                );
            }
            if let InputEvent::Key(key) = self.read_frame(&frame).await? {
                match menu_key(key, first, lines.len(), capacity) {
                    Action::Back | Action::Select => return Ok(()),
                    Action::Move(next) => first = next,
                    _ if matches!(key.code, KeyCode::Char('q' | 'Q')) => return Ok(()),
                    _ => {}
                }
            }
        }
    }

    /// One drawing/input boundary handles interrupts and release events for every screen.
    async fn read_frame(&mut self, frame: &Frame) -> Result<InputEvent> {
        self.draw(frame)?;
        loop {
            let event = tokio::select! {
                biased;
                _ = self.interrupt.recv() => bail!("setup interrupted"),
                _ = self.terminate.recv() => bail!("setup terminated"),
                event = self.events.next() => event.context("terminal input closed")?.context("reading terminal input")?,
            };
            match event {
                InputEvent::Key(key) if common_key(key) == Action::Exit => {
                    bail!("setup interrupted")
                }
                InputEvent::Key(key) if key.kind == KeyEventKind::Release => continue,
                _ => return Ok(event),
            }
        }
    }

    fn draw(&mut self, frame: &Frame) -> Result<()> {
        // A panic in an asynchronous worker runs the process-wide hook before
        // its JoinError reaches the wizard. Do not paint over the main screen
        // if the caller subsequently tries to show an error menu.
        ensure!(
            ACTIVE.load(Ordering::SeqCst),
            "setup screen was restored after an interruption"
        );
        let mut output = io::stdout().lock();
        queue!(
            output,
            Hide,
            SetAttribute(Attribute::Reset),
            ResetColor,
            Clear(ClearType::All)
        )?;
        for row in &frame.lines {
            queue!(
                output,
                MoveTo(frame.left, row.y),
                SetAttribute(Attribute::Reset),
                ResetColor
            )?;
            match row.tone {
                Tone::Title => queue!(
                    output,
                    SetForegroundColor(Color::Cyan),
                    SetAttribute(Attribute::Bold)
                )?,
                Tone::Muted => queue!(output, SetAttribute(Attribute::Dim))?,
                Tone::Selected => queue!(
                    output,
                    SetAttribute(Attribute::Reverse),
                    SetAttribute(Attribute::Bold)
                )?,
                Tone::Normal => {}
            }
            queue!(output, Print(&row.text))?;
        }
        queue!(output, SetAttribute(Attribute::Reset), ResetColor)?;
        if let Some((x, y)) = frame.cursor {
            queue!(output, MoveTo(x, y), Show)?;
        }
        output.flush().context("drawing setup screen")
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if ACTIVE.swap(false, Ordering::SeqCst) {
            let _ = restore_terminal();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Select,
    Back,
    Exit,
    Move(usize),
    Ignore,
}

fn common_key(key: KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::Ignore;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'd' | 'D'))
    {
        return Action::Exit;
    }
    match key.code {
        KeyCode::Enter if key.kind == KeyEventKind::Press => Action::Select,
        KeyCode::Esc => Action::Back,
        _ => Action::Ignore,
    }
}

fn menu_key(key: KeyEvent, selected: usize, count: usize, page: usize) -> Action {
    let common = common_key(key);
    if common != Action::Ignore || key.kind == KeyEventKind::Release || count == 0 {
        return common;
    }
    let last = count - 1;
    let next = match key.code {
        KeyCode::Up => selected.saturating_sub(1),
        KeyCode::Down => selected.saturating_add(1).min(last),
        KeyCode::Home => 0,
        KeyCode::End => last,
        KeyCode::PageUp => selected.saturating_sub(page.max(1)),
        KeyCode::PageDown => selected.saturating_add(page.max(1)).min(last),
        _ => return Action::Ignore,
    };
    Action::Move(next)
}

fn safe_char(character: char) -> bool {
    !character.is_control()
        && !matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
}

pub fn clean(text: &str) -> String {
    text.chars()
        .filter_map(|character| {
            if matches!(character, '\n' | '\r' | '\t') {
                Some(' ')
            } else {
                safe_char(character).then_some(character)
            }
        })
        .collect()
}

fn clip(text: &str, width: usize) -> String {
    let text = clean(text);
    if text.width() <= width {
        return text;
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", prefix(&text, width - 1))
}

fn prefix(text: &str, width: usize) -> &str {
    if text.is_ascii() {
        return &text[..text.len().min(width)];
    }
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if text[..next].width() > width {
            break;
        }
        end = next;
    }
    &text[..end]
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in clean(paragraph).split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if candidate.width() <= width {
                line = candidate;
            } else {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                line = clip(word, width);
            }
        }
        lines.push(line);
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Title,
    Normal,
    Muted,
    Selected,
}
struct Row {
    y: u16,
    text: String,
    tone: Tone,
}
struct Frame {
    lines: Vec<Row>,
    left: u16,
    width: usize,
    height: u16,
    start: u16,
    capacity: usize,
    cursor: Option<(u16, u16)>,
}

impl Frame {
    fn new(cols: u16, rows: u16, title: &str, description: &str, footer: &str) -> Self {
        let left = if cols >= 12 { 2 } else { 0 };
        let width = usize::from(cols.saturating_sub(left * 2 + 1));
        let spacious = rows >= 10;
        let description_limit = if rows >= 16 {
            4
        } else if spacious {
            2
        } else {
            1
        };
        let description_lines: Vec<_> = wrap(description, width)
            .into_iter()
            .take(description_limit)
            .collect();
        let start = if description_lines.is_empty() {
            u16::from(spacious) + 1
        } else if spacious {
            4 + description_lines.len() as u16
        } else if rows >= 5 {
            3
        } else {
            1
        };
        let mut frame = Self {
            lines: Vec::new(),
            left,
            width,
            height: rows,
            start,
            capacity: usize::from(rows.saturating_sub(start + 1)),
            cursor: None,
        };
        frame.line(
            u16::from(spacious),
            format!("logishell · {title}"),
            Tone::Title,
        );
        if rows >= 5 {
            let description_row = if spacious { 3 } else { 1 };
            for (index, line) in description_lines.into_iter().enumerate() {
                frame.line(description_row + index as u16, line, Tone::Muted);
            }
        }
        if rows >= 3 {
            frame.line(rows - 1, footer.to_owned(), Tone::Muted);
        }
        frame
    }

    fn line(&mut self, y: u16, text: String, tone: Tone) {
        if y < self.height && self.width > 0 {
            self.lines.push(Row {
                y,
                text: clip(&text, self.width),
                tone,
            });
        }
    }
}

fn menu_frame(
    cols: u16,
    rows: u16,
    title: &str,
    description: &str,
    choices: &[Choice],
    selected: usize,
    first: &mut usize,
) -> Frame {
    let mut frame = Frame::new(
        cols,
        rows,
        title,
        description,
        &format!("{}/{}   Esc back", selected + 1, choices.len()),
    );
    let capacity = frame.capacity.max(1);
    *first = (*first).min(choices.len().saturating_sub(capacity));
    if selected < *first {
        *first = selected;
    }
    if selected >= first.saturating_add(capacity) {
        *first = selected + 1 - capacity;
    }
    for (offset, choice) in choices.iter().enumerate().skip(*first).take(frame.capacity) {
        let marker = if offset == selected { "›" } else { " " };
        let detail = if choice.detail.is_empty() {
            String::new()
        } else {
            format!("  ·  {}", choice.detail)
        };
        frame.line(
            frame.start + (offset - *first) as u16,
            format!("{marker} {}{detail}", choice.label),
            if offset == selected {
                Tone::Selected
            } else {
                Tone::Normal
            },
        );
    }
    frame
}

struct Editor {
    value: Vec<char>,
    cursor: usize,
}
impl Editor {
    fn new(initial: &str) -> Self {
        let value: Vec<_> = clean(initial).chars().take(MAX_INPUT).collect();
        let cursor = value.len();
        Self { value, cursor }
    }
    fn text(&self) -> String {
        self.value.iter().collect()
    }
    fn insert(&mut self, text: &str) {
        let added: Vec<_> = clean(text)
            .chars()
            .take(MAX_INPUT.saturating_sub(self.value.len()))
            .collect();
        let count = added.len();
        self.value.splice(self.cursor..self.cursor, added);
        self.cursor += count;
    }
    fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(key.code, KeyCode::Char('u' | 'U')) {
                self.value.clear();
                self.cursor = 0;
            }
            return;
        }
        match key.code {
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.value.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.value.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.value.len() => {
                self.value.remove(self.cursor);
            }
            KeyCode::Char(character)
                if safe_char(character)
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SUPER) =>
            {
                self.insert(&character.to_string())
            }
            _ => {}
        }
    }
    fn view(&self, width: usize) -> (String, usize) {
        // Leave a cell for the caret. Character indices keep edits UTF-8 safe.
        let width = width.saturating_sub(1);
        let mut start = self.cursor;
        let mut before = String::new();
        // Work back only through the visible prefix, not the entire input on
        // every keystroke. This also keeps the caret over the actual text.
        for character in self.value[..self.cursor].iter().rev() {
            let candidate = format!("{character}{before}");
            if candidate.width() > width {
                break;
            }
            before = candidate;
            start -= 1;
        }
        let visible: String = self.value[start..].iter().collect();
        (prefix(&visible, width).to_owned(), before.width())
    }
}

fn input_frame(cols: u16, rows: u16, title: &str, description: &str, editor: &Editor) -> Frame {
    let mut frame = Frame::new(cols, rows, title, description, "Esc back   Ctrl+U clear");
    if frame.capacity > 0 && frame.width >= 3 {
        let (text, column) = editor.view(frame.width - 2);
        frame.line(frame.start, format!("> {text}"), Tone::Normal);
        frame.cursor = Some((frame.left + 2 + column as u16, frame.start));
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn navigation_is_bounded_and_release_does_not_activate_items() {
        assert_eq!(menu_key(key(KeyCode::Up), 0, 8, 3), Action::Move(0));
        assert_eq!(menu_key(key(KeyCode::Down), 7, 8, 3), Action::Move(7));
        assert_eq!(menu_key(key(KeyCode::PageDown), 2, 8, 3), Action::Move(5));
        assert_eq!(menu_key(key(KeyCode::End), 0, 8, 3), Action::Move(7));
        assert_eq!(
            menu_key(
                KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release),
                2,
                8,
                3
            ),
            Action::Ignore
        );
        assert_eq!(common_key(key(KeyCode::Esc)), Action::Back);
        assert_eq!(
            common_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Action::Exit
        );
    }

    #[test]
    fn scrolling_and_resize_keep_selection_visible_and_rows_bounded() {
        let choices: Vec<_> = (0..30)
            .map(|index| Choice {
                label: format!("Device {index}"),
                detail: "battery 65%".into(),
            })
            .collect();
        let mut first = 0;
        for (cols, rows, selected) in [(80, 12, 25), (35, 8, 25), (20, 5, 2), (80, 24, 29)] {
            let frame = menu_frame(
                cols,
                rows,
                "Devices",
                "Choose a device",
                &choices,
                selected,
                &mut first,
            );
            assert!(first <= selected && selected < first + frame.capacity);
            assert_eq!(
                frame
                    .lines
                    .iter()
                    .filter(|row| row.tone == Tone::Selected)
                    .count(),
                1
            );
            assert!(
                frame
                    .lines
                    .iter()
                    .all(|row| row.y < rows && row.text.width() <= frame.width)
            );
        }
        for (cols, rows) in [(0, 0), (1, 1), (2, 2)] {
            let frame = menu_frame(cols, rows, "Devices", "", &choices, 0, &mut first);
            assert!(
                frame
                    .lines
                    .iter()
                    .all(|row| row.y < rows && row.text.width() <= frame.width)
            );
        }
    }

    #[test]
    fn taller_screens_show_four_description_lines_without_crowding_choices() {
        let description = "Setting explanation\nCurrent device value\nEnter adds to draft\nSave when finished\nFifth line omitted";
        let frame = Frame::new(80, 16, "Setting", description, "Footer");
        assert_eq!(frame.start, 8);
        assert!(frame.capacity >= 3);
        let explanations: Vec<_> = frame
            .lines
            .iter()
            .filter(|row| (3..=6).contains(&row.y))
            .map(|row| row.text.as_str())
            .collect();
        assert_eq!(
            explanations,
            vec![
                "Setting explanation",
                "Current device value",
                "Enter adds to draft",
                "Save when finished"
            ]
        );
        assert!(
            frame
                .lines
                .iter()
                .any(|row| row.y == 15 && row.text == "Footer")
        );
        assert!(!frame.lines.iter().any(|row| row.text.contains("Fifth")));

        let compact = Frame::new(80, 15, "Setting", description, "Footer");
        assert_eq!(compact.start, 6);
        assert!(
            !compact
                .lines
                .iter()
                .any(|row| row.text.contains("Enter adds"))
        );
        let wrapped = Frame::new(
            22,
            16,
            "Setting",
            "one two three four five six seven eight nine ten eleven twelve",
            "Footer",
        );
        assert_eq!(
            wrapped
                .lines
                .iter()
                .filter(|row| (3..=6).contains(&row.y))
                .count(),
            4
        );
        assert!(wrapped.capacity >= 3);
        assert!(
            wrapped
                .lines
                .iter()
                .all(|row| row.text.width() <= wrapped.width)
        );
    }

    #[test]
    fn untrusted_labels_cannot_inject_terminal_commands_and_unicode_fits() {
        let text = "MX\x1b[2J\n鼠标\u{202e}\x07";
        assert!(
            !clean(text)
                .chars()
                .any(|character| character.is_control() || character == '\u{202e}')
        );
        for width in 0..15 {
            let output = clip(text, width);
            assert!(output.width() <= width);
            assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        }
        assert_eq!(clip("e\u{301}猫", 2), "e\u{301}…");
        assert_eq!(clip("hello", 3), "he…");
        assert_eq!(clip("hello", 0), "");
    }

    #[test]
    fn input_edits_unicode_safely_and_paste_never_submits_or_injects_controls() {
        let mut editor = Editor::new("A猫B");
        editor.key(key(KeyCode::Left));
        editor.key(key(KeyCode::Backspace));
        editor.insert("é\n\x1b[2J\u{202e}");
        assert_eq!(editor.text(), "Aé [2JB");
        editor.key(key(KeyCode::Home));
        editor.key(key(KeyCode::Delete));
        assert_eq!(editor.text(), "é [2JB");
        editor.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "");
        editor.insert(&"猫".repeat(MAX_INPUT + 1));
        assert_eq!(editor.value.len(), MAX_INPUT);
        for width in 0..15 {
            let (text, cursor) = editor.view(width);
            assert!(text.width() <= width.saturating_sub(1));
            assert!(cursor <= width.saturating_sub(1));
        }
    }
}
