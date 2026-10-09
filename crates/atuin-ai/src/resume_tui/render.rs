//! Drawing the picker. The layout, header, tabs, input box and borders follow the history search
//! (`atuin search -i`) exactly, so the two feel like one tool.

use std::borrow::Cow;
use std::ops::Range;
use std::path::Path;

use atuin_client::ai_session::{HarnessKind, HarnessSession};
use atuin_client::settings::{
    AiSessionFilterMode as FilterMode, KeymapMode, PreviewStrategy, Settings, Style as UiStyle,
};
use atuin_client::theme::{Meaning, Theme};
use atuin_common::string::ellipsis::{Indicator, Pos};
use atuin_common::string::{Alignment as Align, EllipsizeExt as _, Measure};
use atuin_common::time::OffsetDateTimeExt as _;
use ratatui::backend::FromCrossterm;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    StatefulWidget, Tabs, Widget,
};
use ratatui::{Frame, symbols};
use time::{OffsetDateTime, UtcOffset};
use unicode_width::UnicodeWidthStr;

use super::chooser::ListAnchor;
use super::panel::{self, SPLIT_MIN_WIDTH};
use super::query::{TokenKind, TokenState};
use super::resumer::shell_line;
use super::source::{SessionRow, Snippet, harness_badge, harness_label};
use super::state::{LIVE_SECS, ListState, Pane, Pending, SEARCH_LIMIT, State, TAB_TITLES};
use super::{clock, markdown};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum Compactness {
    Ultracompact,
    Compact,
    Full,
}

pub fn to_compactness(area: Rect, settings: &Settings) -> Compactness {
    if match settings.style {
        UiStyle::Auto => area.height < 14,
        UiStyle::Compact => true,
        UiStyle::Full => false,
    } {
        if settings.auto_hide_height != 0 && area.height <= settings.auto_hide_height {
            Compactness::Ultracompact
        } else {
            Compactness::Compact
        }
    } else {
        Compactness::Full
    }
}

pub(super) fn style(theme: &Theme, meaning: Meaning) -> Style {
    Style::from_crossterm(theme.as_style(meaning))
}

/// Collapse whitespace and control characters to single spaces, remembering each output byte's
/// source byte so highlight ranges still apply.
fn flatten(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len());
    let mut space = false;
    for (i, c) in text.trim().char_indices() {
        let offset = i + (text.len() - text.trim_start().len());
        if c.is_whitespace() || c.is_control() {
            if !space {
                out.push(' ');
                map.push(offset);
            }
            space = true;
        } else {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            out.push_str(s);
            map.extend(std::iter::repeat_n(offset, s.len()));
            space = false;
        }
    }
    (out, map)
}

/// Group characters into spans by whether their source byte is highlighted.
fn spans_from(
    chars: impl Iterator<Item = (char, Option<usize>)>,
    map: &[usize],
    highlights: &[Range<usize>],
    base: Style,
    hl: Style,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_hl = false;
    for (ch, source) in chars {
        let is_hl = source
            .and_then(|b| map.get(b))
            .is_some_and(|src| highlights.iter().any(|r| r.contains(src)));
        if is_hl != run_hl && !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if run_hl {
                    hl
                } else {
                    base
                },
            ));
        }
        run_hl = is_hl;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(
            run,
            if run_hl {
                hl
            } else {
                base
            },
        ));
    }
    spans
}

/// One line of `text` fitted to `width` columns, with the `highlights` (byte ranges into `text`)
/// drawn in `hl`.
fn highlighted_line(
    text: &str,
    highlights: &[Range<usize>],
    width: usize,
    base: Style,
    hl: Style,
) -> Vec<Span<'static>> {
    let (flat, map) = flatten(text);
    let ellipsized = flat.ellipsize(Measure::Columns(width), Pos::End, Indicator::UNICODE);
    let display = ellipsized.to_string();
    let chars = display.char_indices().map(|(i, c)| (c, ellipsized.source_index(i)));
    spans_from(chars, &map, highlights, base, hl)
}

/// The repository (or directory) name for the repo column.
pub(super) fn repo_name(row: &SessionRow) -> String {
    row.git_root
        .as_deref()
        .or(row.cwd.as_deref())
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(super) fn is_live(now: OffsetDateTime, row: &SessionRow) -> bool {
    now.saturating_duration_since(row.updated_at).as_secs() < LIVE_SECS
}

/// The branch a row shows: none when detached (`HEAD`).
pub(super) fn shown_branch(row: &SessionRow) -> Option<&str> {
    row.branch.as_deref().filter(|b| !b.is_empty() && *b != "HEAD")
}

/// `1234`, or `12k` once it no longer fits the messages column.
fn message_count(n: u64) -> String {
    if n < 10_000 {
        n.to_string()
    } else {
        panel::human(n)
    }
}

// --- the row layout --------------------------------------------------------------------------

/// A column in the session rows' first line, left to right after the selection indicator.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Column {
    /// When the session was last updated: `12m` or `3h` while recent, then a clock time
    /// (`14:02`, `yest 09:40`, `Mon 09:40`, `Sep 27`, `2025-09-27`). One-line rows only: a
    /// two-line row says it under its title, where it reads after the title.
    Time,
    /// The harness badge: CC, CX, OC or PI. One-line rows only: two-line rows name the agent on
    /// their second line.
    Harness,
    /// The session title. Expands to fill the row.
    Title,
    /// The message count, left out when the title would be short of room.
    Messages,
}

impl Column {
    /// Width in cells. The title expands instead.
    fn width(self) -> u16 {
        match self {
            Self::Time => u16::try_from(clock::WIDTH).unwrap_or(u16::MAX),
            Self::Harness => 2,
            Self::Title => 0,
            Self::Messages => 4,
        }
    }
}

/// How the list lays its rows out.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RowShape {
    /// Two lines a row: the title, then when, the agent and where the session ran. One line
    /// (with time and harness badge columns) when the picker is ultracompact.
    pub two_line: bool,
    /// Grouped under a heading for each day (the list is newest first, with no query text to
    /// rank by), so a row's time says only the time of day.
    pub by_day: bool,
}

impl RowShape {
    /// The lines a row takes.
    pub fn height(self) -> usize {
        if self.two_line {
            2
        } else {
            1
        }
    }

    fn columns(self) -> &'static [Column] {
        if self.two_line {
            &[Column::Title, Column::Messages]
        } else {
            &[Column::Time, Column::Harness, Column::Title, Column::Messages]
        }
    }

    /// Where the title starts, past the indicator and the columns before it.
    pub fn title_x(self) -> u16 {
        3 + self
            .columns()
            .iter()
            .take_while(|c| **c != Column::Title)
            .map(|c| c.width() + 1)
            .sum::<u16>()
    }
}

/// The title keeps at least this many columns while the message count can give them up.
pub const TITLE_MIN: u16 = 30;

/// The columns of rows `width` columns wide (the selection indicator included), each with its
/// width. The title takes what's left, and the message count goes when that would leave the
/// title under [`TITLE_MIN`].
pub fn row_layout(width: u16, shape: RowShape) -> Vec<(Column, u16)> {
    let mut cells = shape.columns().to_vec();
    // Past the indicator, and a space between cells.
    let title_width = |cells: &[Column]| {
        let others: u16 = cells.iter().map(|c| c.width()).sum();
        let gaps = u16::try_from(cells.len().saturating_sub(1)).unwrap_or(u16::MAX);
        width.saturating_sub(3).saturating_sub(others).saturating_sub(gaps)
    };
    if title_width(&cells) < TITLE_MIN {
        cells.retain(|c| *c != Column::Messages);
    }
    let title = title_width(&cells);
    cells
        .into_iter()
        .map(|c| {
            (
                c,
                if c == Column::Title {
                    title
                } else {
                    c.width()
                },
            )
        })
        .collect()
}

pub(super) fn harness_style(theme: &Theme, harness: HarnessKind) -> Style {
    let meaning = match harness {
        HarnessKind::ClaudeCode => Meaning::AlertWarn,
        HarnessKind::Codex => Meaning::AlertInfo,
        HarnessKind::Opencode => Meaning::Guidance,
        HarnessKind::Pi => Meaning::Important,
        HarnessKind::Copilot | HarnessKind::Unknown => Meaning::Annotation,
    };
    style(theme, meaning).add_modifier(Modifier::BOLD)
}

// --- the session list ------------------------------------------------------------------------

/// A line of the list, in order from the input outward (down from it when inverted, up from it
/// otherwise).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListLine {
    /// The heading over a day's sessions: the day of the row at this index.
    Heading(usize),
    /// A row's first line: its time, title and message count.
    Title(usize),
    /// A two-line row's second line: the agent, and where the session ran.
    Place(usize),
    /// A line of the chooser, open under the row it's choosing for: the row, and which line.
    Choice(usize, usize),
    /// The top (`true`) or bottom edge of the border around a row with the chooser open.
    Edge(usize, bool),
}

/// The list's lines from the input outward, and where each row's first line is among them.
/// `chooser` is the row the chooser is open under, and how many lines it has.
///
/// Read top to bottom on screen, a day's heading is over its sessions, and a row's title over
/// its place and then the chooser (the three in a border), whichever way up the list is: so
/// outward from an input under the list, the border's bottom edge and the chooser come first
/// (last line first), then the row's place, then its title, and a day's heading after its
/// sessions.
pub fn list_lines(
    rows: &[SessionRow],
    shape: RowShape,
    inverted: bool,
    chooser: Option<(usize, usize)>,
    day_of: impl Fn(&SessionRow) -> time::Date,
) -> (Vec<ListLine>, Vec<usize>) {
    let mut lines = Vec::with_capacity(rows.len() * (shape.height() + 1));
    let mut starts = Vec::with_capacity(rows.len());
    let mut i = 0;
    while i < rows.len() {
        // The rows of one day (all of them, when not grouped).
        let end = if shape.by_day {
            let day = day_of(&rows[i]);
            i + rows[i..].iter().take_while(|r| day_of(r) == day).count()
        } else {
            rows.len()
        };
        if shape.by_day && inverted {
            lines.push(ListLine::Heading(i));
        }
        for row in i..end {
            starts.push(lines.len());
            // Top to bottom on screen.
            let mut row_lines = vec![ListLine::Title(row)];
            if shape.two_line {
                row_lines.push(ListLine::Place(row));
            }
            if let Some((at, n)) = chooser
                && at == row
            {
                row_lines.insert(0, ListLine::Edge(row, true));
                row_lines.extend((0..n).map(|k| ListLine::Choice(row, k)));
                row_lines.push(ListLine::Edge(row, false));
            }
            if !inverted {
                row_lines.reverse();
            }
            lines.extend(row_lines);
        }
        if shape.by_day && !inverted {
            lines.push(ListLine::Heading(i));
        }
        i = end;
    }
    (lines, starts)
}

pub struct SessionList<'a> {
    rows: &'a [SessionRow],
    block: Option<Block<'a>>,
    inverted: bool,
    alternate_highlight: bool,
    now: OffsetDateTime,
    tz: UtcOffset,
    indicator: &'a str,
    theme: &'a Theme,
    cells: &'a [(Column, u16)],
    shape: RowShape,
    /// This host's id, left out of the rows' places.
    here: &'a str,
    /// The chooser's lines, when it's open under the selected row, and which of them is
    /// selected.
    chooser: Option<(&'a [Line<'static>], usize)>,
    /// The columns of the row the chooser is open under, inside its border.
    boxed_cells: &'a [(Column, u16)],
}

impl SessionList<'_> {
    /// The first line to show of `total`, `height` at a time, so that the selected row (spanning
    /// `lo..hi`, its day's heading included when that is beside it) shows with some of the rows
    /// around it, moving from `offset` only as far as that takes.
    fn scroll_to(offset: usize, lo: usize, hi: usize, height: usize, total: usize) -> usize {
        let margin = (height.saturating_sub(hi - lo) / 2).min(4);
        let mut offset = offset;
        if hi + margin > offset + height {
            offset = (hi + margin).saturating_sub(height);
        }
        if lo < offset + margin {
            offset = lo.saturating_sub(margin);
        }
        offset.min(total.saturating_sub(height)).min(lo)
    }
}

impl StatefulWidget for SessionList<'_> {
    type State = ListState;

    fn render(mut self, area: Rect, buf: &mut Buffer, state: &mut ListState) {
        let list_area = self.block.take().map_or(area, |b| {
            let inner = b.inner(area);
            b.render(area, buf);
            inner
        });
        let height = usize::from(list_area.height);
        state.selected_line = None;
        if list_area.width < 1 || list_area.height < 1 || self.rows.is_empty() {
            state.max_entries = height / self.shape.height();
            state.lines = 0;
            return;
        }
        state.selected = state.selected.min(self.rows.len() - 1);
        let (now, tz) = (self.now, self.tz);
        let chooser = self.chooser.map(|(c, _)| (state.selected, c.len()));
        let (lines, starts) = list_lines(self.rows, self.shape, self.inverted, chooser, |r| {
            clock::day(r.updated_at, tz)
        });

        // The selected row (the chooser and its border included), with its day's heading when
        // that is beside it.
        let start = starts[state.selected];
        let end = start + self.shape.height() + chooser.map_or(0, |(_, n)| n + 2);
        let heading = |at: Option<&ListLine>| matches!(at, Some(ListLine::Heading(_)));
        let mut lo = start - usize::from(start > 0 && heading(lines.get(start - 1)));
        let mut hi = end + usize::from(heading(lines.get(end)));
        // Too tall to show whole, the row with the chooser under it keeps the chooser's selected
        // line in sight instead.
        if hi - lo > height
            && let Some((_, k)) = self.chooser
            && let Some(at) = lines.iter().position(|l| *l == ListLine::Choice(state.selected, k))
        {
            (lo, hi) = (at, at + 1);
        }
        state.offset = SessionList::scroll_to(state.offset, lo, hi, height, lines.len());
        state.max_entries = (height / self.shape.height()).max(1);
        state.lines = lines.len();

        for (k, line) in lines.iter().enumerate().skip(state.offset).take(height) {
            let screen_y = u16::try_from(k - state.offset).unwrap_or(u16::MAX);
            let y = if self.inverted {
                list_area.top() + screen_y
            } else {
                list_area.bottom() - screen_y - 1
            };
            let (ListLine::Heading(row)
            | ListLine::Title(row)
            | ListLine::Place(row)
            | ListLine::Choice(row, _)
            | ListLine::Edge(row, _)) = *line;
            let selected = row == state.selected && !matches!(line, ListLine::Heading(_));
            // The row the chooser is open under, in a border: that says it's the one chosen.
            let boxed = selected && self.chooser.is_some();
            let border = style(self.theme, Meaning::Base);
            let mut w = RowWriter {
                buf: &mut *buf,
                x: list_area.left(),
                right: list_area.right(),
                y,
                // Another host's session looks like any other: it resumes by being restored
                // from sync, behind the scenes.
                row_modifier: if self.alternate_highlight && selected {
                    Modifier::REVERSED
                } else {
                    Modifier::empty()
                },
            };
            if boxed {
                // Inside the border, with a space before its right edge.
                w.right = list_area.right().saturating_sub(2);
                w.row_modifier = Modifier::empty();
            }
            match line {
                ListLine::Edge(_, top) => {
                    w.right = list_area.right();
                    let (l, r) = if *top {
                        ("╭", "╮")
                    } else {
                        ("╰", "╯")
                    };
                    let across = usize::from(list_area.width.saturating_sub(2));
                    w.put(&format!("{l}{}{r}", symbols::line::HORIZONTAL.repeat(across)), border);
                }
                ListLine::Heading(_) => {
                    let day = clock::day(self.rows[row].updated_at, tz);
                    self.render_heading(&mut w, &clock::day_heading(now, day, tz));
                }
                ListLine::Title(_) => {
                    if selected {
                        state.selected_line = Some(y);
                    }
                    self.render_row(&mut w, &self.rows[row], selected, boxed);
                }
                ListLine::Place(_) => self.render_place(&mut w, &self.rows[row]),
                ListLine::Choice(_, k) => {
                    // Under the title, and clear of the selection's highlight.
                    w.row_modifier = Modifier::empty();
                    w.pad_to(w.x + self.shape.title_x());
                    if let Some(line) = self.chooser.and_then(|(c, _)| c.get(*k)) {
                        w.put_spans(&line.spans);
                    }
                }
            }
            if boxed && !matches!(line, ListLine::Edge(..)) {
                let right = list_area.right().saturating_sub(1);
                buf[(list_area.left(), y)].set_symbol("│").set_style(border);
                buf[(right, y)].set_symbol("│").set_style(border);
            }
        }
    }
}

struct RowWriter<'b> {
    buf: &'b mut Buffer,
    x: u16,
    right: u16,
    y: u16,
    row_modifier: Modifier,
}

impl RowWriter<'_> {
    fn put(&mut self, s: &str, style: Style) {
        if self.x >= self.right {
            return;
        }
        let w = usize::from(self.right - self.x);
        let (x, _) =
            self.buf.set_stringn(self.x, self.y, s, w, style.add_modifier(self.row_modifier));
        self.x = x;
    }

    fn put_spans(&mut self, spans: &[Span<'_>]) {
        for span in spans {
            self.put(&span.content, span.style);
        }
    }

    /// Pad with spaces up to column `to`, so the next column lines up.
    fn pad_to(&mut self, to: u16) {
        while self.x < to.min(self.right) {
            self.put(" ", Style::default());
        }
    }
}

impl SessionList<'_> {
    /// A day's heading: its name, then a rule to the edge.
    fn render_heading(&self, w: &mut RowWriter<'_>, day: &str) {
        let muted = style(self.theme, Meaning::Annotation);
        w.put(" ", Style::default());
        w.put(day, style(self.theme, Meaning::Important).add_modifier(Modifier::BOLD));
        w.put(" ", Style::default());
        let rule = "─".repeat(usize::from(w.right.saturating_sub(w.x)));
        w.put(&rule, muted);
    }

    /// A two-line row's second line, under its title, dimmed: when (a live session's dot in its
    /// colour), the agent, then where the session ran (`14:02 · Claude Code ·
    /// atuin · main · @3f9a12bc`).
    fn render_place(&self, w: &mut RowWriter<'_>, row: &SessionRow) {
        // Dimmed, the agent too, so the eye goes to the titles; only a live session's dot stands
        // out.
        let muted = style(self.theme, Meaning::Annotation).add_modifier(Modifier::DIM);
        w.pad_to(w.x + self.shape.title_x());
        if is_live(self.now, row) {
            w.put("● ", style(self.theme, Meaning::AlertInfo));
        }
        w.put(&self.when(row), muted);
        w.put(" · ", muted);
        w.put(harness_label(row.handle.harness), muted);
        for part in panel::place(row, self.here, self.theme) {
            if part.content != " · " {
                w.put(" · ", muted);
                w.put(&part.content, muted);
            }
        }
    }

    /// When `row` was last active: the time of day under a day's heading, else `12m`, `14:02`,
    /// `yest 09:40`, … (see [`clock::When::short`]).
    fn when(&self, row: &SessionRow) -> String {
        if self.shape.by_day {
            clock::time_in_day(self.now, row.updated_at, self.tz)
        } else {
            clock::When::of(self.now, row.updated_at, self.tz).short().to_owned()
        }
    }

    /// A row's first line. `boxed`, it's the row the chooser is open under, which its border
    /// marks in place of the indicator and the selected title's colour.
    fn render_row(&self, w: &mut RowWriter<'_>, row: &SessionRow, selected: bool, boxed: bool) {
        let theme = self.theme;
        w.put(
            if selected && !boxed {
                self.indicator
            } else {
                "   "
            },
            Style::default(),
        );
        let cells = if boxed {
            self.boxed_cells
        } else {
            self.cells
        };

        let pad = |text: &str, cw: usize, align: Align| {
            text.pad_ellipsize(Measure::Columns(cw), Pos::End, Indicator::UNICODE, align)
                .into_owned()
        };
        for (idx, &(cell, col_width)) in cells.iter().enumerate() {
            if idx != 0 {
                w.put(" ", Style::default());
            }
            let end = w.x.saturating_add(col_width);
            let cw = usize::from(col_width);
            match cell {
                Column::Time => {
                    let when = self.when(row);
                    let (text, meaning) = if is_live(self.now, row) {
                        (format!("● {when}"), Meaning::AlertInfo)
                    } else {
                        (when, Meaning::Annotation)
                    };
                    w.put(&pad(&text, cw, Align::End), style(theme, meaning));
                }
                Column::Harness => {
                    w.put(
                        harness_badge(row.handle.harness),
                        harness_style(theme, row.handle.harness),
                    );
                }
                Column::Title => {
                    let (base, hl) = if boxed {
                        let base = style(theme, Meaning::Base).add_modifier(Modifier::BOLD);
                        (base, style(theme, Meaning::AlertWarn).add_modifier(Modifier::BOLD))
                    } else if selected && !self.alternate_highlight {
                        let base = style(theme, Meaning::AlertError).add_modifier(Modifier::BOLD);
                        (base, style(theme, Meaning::AlertWarn).add_modifier(Modifier::BOLD))
                    } else {
                        let base = style(theme, Meaning::Base);
                        (base, base.add_modifier(Modifier::BOLD))
                    };
                    let spans =
                        highlighted_line(&row.title.text, &row.title.highlights, cw, base, hl);
                    w.put_spans(&spans);
                }
                Column::Messages => {
                    w.put(
                        &format!("{:>cw$}", message_count(row.messages)),
                        style(theme, Meaning::Annotation),
                    );
                }
            }
            w.pad_to(end);
        }
    }
}

// --- the frame -------------------------------------------------------------------------------

/// The width of the preview's `first  ` / `match  ` / `last   ` labels.
const PREVIEW_LABEL_WIDTH: usize = 7;

/// One preview part, ready to render as markdown.
struct PreviewSource<'a> {
    label: &'static str,
    text: Cow<'a, str>,
    highlights: Vec<Range<usize>>,
    styles: markdown::Styles,
}

struct PreviewParts<'a> {
    loaded: bool,
    first: Option<(&'a str, Vec<Range<usize>>)>,
    matched: Option<&'a Snippet>,
    last: Option<(&'a str, Vec<Range<usize>>)>,
}

/// `text` with the match's highlights when the match lies inside it (taking the match, so it
/// isn't shown twice).
fn locate<'a>(
    text: Option<&'a str>,
    matched: &mut Option<&Snippet>,
) -> Option<(&'a str, Vec<Range<usize>>)> {
    let text = text?;
    let highlights = match *matched {
        Some(m) if !m.text.is_empty() => text.find(m.text.as_str()).map(|at| {
            *matched = None;
            shift(&m.highlights, at)
        }),
        _ => None,
    };
    Some((text, highlights.unwrap_or_default()))
}

fn shift(ranges: &[Range<usize>], by: usize) -> Vec<Range<usize>> {
    ranges.iter().map(|r| r.start + by..r.end + by).collect()
}

/// How many lines of a pane's text in full are rendered at a time as it scrolls: the rendering
/// grows by this much as the view nears its end, so a long reply is never rendered whole just to
/// show its first screen.
const WINDOW_CHUNK: usize = 128;

/// What a scrolling pane shows.
pub(super) struct Body {
    pub lines: Vec<Line<'static>>,
    /// The first line shown of the text in full; 0 shows the overview.
    pub offset: usize,
    /// The lines of the text in full rendered.
    pub len: usize,
    /// More lines than `len`, not rendered yet.
    pub more: bool,
}

impl Body {
    fn overflows(&self, height: usize) -> bool {
        self.more || self.len > height
    }
}

/// `height` lines of a pane's text, scrolled to `want`: the `overview` at the top, else the text
/// in full from there (`document`, asked for enough lines to fill the view and more), stopping at
/// its end.
pub(super) fn scroll_body(
    want: usize,
    height: usize,
    overview: impl FnOnce() -> Vec<Line<'static>>,
    document: impl FnOnce(usize) -> (Vec<Line<'static>>, bool),
) -> Body {
    let limit = (want + 2 * height).max(1).div_ceil(WINDOW_CHUNK) * WINDOW_CHUNK;
    let (doc, more) = document(limit);
    let len = doc.len();
    let offset = want.min(len.saturating_sub(height));
    let lines = if offset == 0 {
        overview()
    } else {
        doc.into_iter().skip(offset).take(height).collect()
    };
    Body {
        lines,
        offset,
        len,
        more,
    }
}

/// A scrollbar in `track` (a border, or a column kept for it) for `body` shown `height` lines at
/// a time, once it has more than fits.
fn draw_scrollbar(f: &mut Frame, track: Rect, body: &Body, height: usize, theme: &Theme) {
    if !body.overflows(height) || track.height == 0 {
        return;
    }
    // The text not rendered yet counts for a screen more, so the thumb never says it's at the end.
    let len = body.len
        + if body.more {
            height
        } else {
            0
        };
    let mut state = ScrollbarState::new(len.saturating_sub(height))
        .position(body.offset)
        .viewport_content_length(height);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .thumb_symbol("┃")
        .style(style(theme, Meaning::Annotation));
    f.render_stateful_widget(bar, track, &mut state);
}

#[derive(Clone, Copy)]
struct StyleState {
    compactness: Compactness,
    invert: bool,
    inner_width: usize,
}

impl State {
    /// The selected session's preview parts: the first prompt, the match (unless it lies inside
    /// the first prompt or the last reply, which then carry its highlights), and the last reply.
    fn preview_parts(&self) -> Option<PreviewParts<'_>> {
        let row = self.preview_row()?;
        let preview = self.previews.get(&row.handle);
        let mut matched = row.matched.as_ref();
        let first = locate(preview.and_then(|p| p.first_prompt.as_deref()), &mut matched);
        let last = locate(preview.and_then(|p| p.last_assistant.as_deref()), &mut matched);
        Some(PreviewParts {
            loaded: preview.is_some(),
            first,
            matched,
            last,
        })
    }

    /// The selected session's preview parts as markdown sources: a label, the text, its
    /// highlights, and the style it runs in.
    fn preview_sources(&self, theme: &Theme) -> Option<(bool, Vec<PreviewSource<'_>>)> {
        let parts = self.preview_parts()?;
        let base = style(theme, Meaning::Base);
        let muted = style(theme, Meaning::Annotation);
        let mut sources = Vec::new();
        if let Some((text, highlights)) = parts.first {
            sources.push(PreviewSource {
                label: "first  ",
                text: Cow::Borrowed(text),
                highlights,
                styles: markdown::Styles::new(theme, base),
            });
        }
        if let Some(Snippet { text, highlights }) = parts.matched {
            sources.push(PreviewSource {
                label: "match  ",
                text: Cow::Owned(format!("…{text}")),
                highlights: shift(highlights, '…'.len_utf8()),
                styles: markdown::Styles::new(theme, base),
            });
        }
        if let Some((text, highlights)) = parts.last {
            sources.push(PreviewSource {
                label: "last   ",
                text: Cow::Borrowed(text),
                highlights,
                styles: markdown::Styles::new(theme, muted),
            });
        }
        Some((parts.loaded, sources))
    }

    /// Each preview part rendered at `width` in at most `height` lines.
    fn preview_rendered(
        sources: &[PreviewSource<'_>],
        width: usize,
        height: usize,
    ) -> Vec<Vec<Line<'static>>> {
        let opts = markdown::Opts {
            width: width.saturating_sub(PREVIEW_LABEL_WIDTH),
            max_lines: height,
            spacing: false,
            urls: false,
        };
        sources.iter().map(|s| markdown::render(&s.text, &s.highlights, opts, &s.styles)).collect()
    }

    /// The preview's line saying what forked off the session previewed, in the text column:
    /// `2 forks`. (Where it ran is under its title in the list.) `None` when nothing did, or
    /// there's no room beside the text (`height` under 2).
    fn preview_meta(&self, height: usize, theme: &Theme) -> Option<Line<'static>> {
        if height < 2 {
            return None;
        }
        let row = self.preview_row()?;
        let forks = panel::forks(self.children.get(&row.handle).map(Vec::as_slice))?;
        Some(Line::from(vec![
            Span::raw(" ".repeat(PREVIEW_LABEL_WIDTH)),
            Span::styled(forks, style(theme, Meaning::Annotation)),
        ]))
    }

    /// The strip's overview of the parts, in `height` lines of `width` columns: they share the
    /// lines, and a part with only one gets its markdown run onto that line.
    fn preview_overview(
        sources: &[PreviewSource<'_>],
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let label = |s: &'static str| Span::styled(s, style(theme, Meaning::Annotation));
        let inner = width.saturating_sub(PREVIEW_LABEL_WIDTH);
        let rendered = Self::preview_rendered(sources, width, height);
        let wants: Vec<usize> = rendered.iter().map(Vec::len).collect();
        let budgets = markdown::allocate(&wants, height);
        let mut lines: Vec<Line<'static>> = Vec::new();

        for ((source, rendered), n) in sources.iter().zip(&rendered).zip(budgets) {
            let body = match n {
                0 => continue,
                1 => vec![markdown::render_flat(
                    &source.text,
                    &source.highlights,
                    inner,
                    &source.styles,
                )],
                n => markdown::fit(rendered, n, inner, source.styles.muted),
            };
            for (i, line) in body.into_iter().enumerate() {
                let mut spans = vec![if i == 0 {
                    label(source.label)
                } else {
                    Span::raw(" ".repeat(PREVIEW_LABEL_WIDTH))
                }];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }
        }
        lines
    }

    /// The strip's parts in full, one after another under their labels, as it scrolls: up to
    /// `limit` lines a part, stopping at the first with more (`true`).
    fn preview_document(
        sources: &[PreviewSource<'_>],
        width: usize,
        limit: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, bool) {
        let opts = markdown::Opts {
            width: width.saturating_sub(PREVIEW_LABEL_WIDTH),
            max_lines: limit,
            spacing: false,
            urls: false,
        };
        let mut lines = Vec::new();
        for source in sources {
            let (body, more) =
                markdown::render_window(&source.text, &source.highlights, opts, &source.styles);
            for (i, line) in body.into_iter().enumerate() {
                let mut spans = vec![if i == 0 {
                    Span::styled(source.label, style(theme, Meaning::Annotation))
                } else {
                    Span::raw(" ".repeat(PREVIEW_LABEL_WIDTH))
                }];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }
            if more {
                return (lines, true);
            }
        }
        (lines, false)
    }

    /// The strip's lines, in `height` lines of `width` columns: the metadata line (see
    /// [`Self::preview_meta`]), then the parts, scrolled (see [`scroll_body`]). Also the session
    /// shown, where the parts start and what scrolling them shows, for the caller to note.
    fn strip_lines(
        &self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Option<(HarnessSession, usize, Body)>) {
        let (Some(row), Some((loaded, sources))) =
            (self.preview_row(), self.preview_sources(theme))
        else {
            return (Vec::new(), None);
        };
        let meta = self.preview_meta(height, theme);
        let top = usize::from(meta.is_some());
        let height = height.saturating_sub(top);
        let want = self.scrolls[Pane::Strip as usize].offset_for(&row.handle);
        let body = scroll_body(
            want,
            height,
            || Self::preview_overview(&sources, width, height, theme),
            |limit| Self::preview_document(&sources, width, limit, theme),
        );
        let mut lines: Vec<Line<'static>> = meta.into_iter().collect();
        lines.extend(body.lines.iter().cloned());
        if !loaded && lines.len() == top {
            lines.push(Line::from(Span::styled("…", style(theme, Meaning::Annotation))));
        }
        (lines, Some((row.handle.clone(), top, body)))
    }

    /// Note how `pane` was drawn: for `session`, in `area` (what the mouse wheel scrolls), with
    /// `height` lines of text showing `body`.
    fn drawn(
        &mut self,
        pane: Pane,
        session: HarnessSession,
        area: Rect,
        height: usize,
        body: &Body,
    ) {
        let scroll = &mut self.scrolls[pane as usize];
        scroll.session = Some(session);
        scroll.offset = body.offset;
        scroll.area = Some(area);
        scroll.height = height;
        scroll.len = body.len;
        scroll.more = body.more;
    }

    fn calc_preview_height(
        &self,
        settings: &Settings,
        compactness: Compactness,
        border_size: u16,
        width: usize,
        theme: &Theme,
    ) -> u16 {
        if settings.show_preview && self.tab_index == 0 {
            let wanted = match settings.preview.strategy {
                PreviewStrategy::Fixed => settings.max_preview_height,
                PreviewStrategy::Static => 3,
                PreviewStrategy::Auto => self.preview_sources(theme).map_or(1, |(_, sources)| {
                    let max = usize::from(settings.max_preview_height);
                    // The metadata line takes the place of one of the text's.
                    let meta = usize::from(self.preview_meta(max, theme).is_some());
                    let lines: usize =
                        Self::preview_rendered(&sources, width, max.saturating_sub(meta))
                            .iter()
                            .map(Vec::len)
                            .sum();
                    // The text gets a line even before it is read (for its `…`).
                    u16::try_from(lines.max(1) + meta).unwrap_or(u16::MAX)
                }),
            };
            wanted.min(settings.max_preview_height).max(1) + border_size * 2
        } else if compactness != Compactness::Full || self.tab_index == 1 {
            0
        } else {
            1
        }
    }

    /// Where the selected row of the list in `area` is, for the chooser to open against.
    fn anchor(&self, area: Rect, shape: RowShape) -> Option<ListAnchor> {
        if self.results.is_empty() || area.height == 0 {
            return None;
        }
        // One-line rows have a badge column past the indicator and the time; two-line rows line
        // the chooser's names up with their titles.
        let name_x = if shape.two_line {
            area.x + shape.title_x()
        } else {
            area.x + 4 + Column::Time.width()
        };
        Some(ListAnchor {
            list: area,
            row: self.list.selected_line?,
            name_x,
        })
    }

    pub fn draw(&mut self, f: &mut Frame, settings: &Settings, theme: &Theme) {
        self.draw_main(f, settings, theme);
        // A popup only where the list didn't open it under the row.
        if self.chooser.is_some() && !self.chooser_inline {
            self.draw_chooser(f, settings, theme);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn draw_main(&mut self, f: &mut Frame, settings: &Settings, theme: &Theme) {
        let area = f.area();
        self.list_anchor = None;
        self.chooser_inline = false;
        for scroll in &mut self.scrolls {
            scroll.area = None;
        }
        f.render_widget(Clear, area);
        let compactness = to_compactness(area, settings);
        let invert = settings.invert;
        let border_size = u16::from(compactness == Compactness::Full);
        // Wide terminals get the detail pane beside the list instead of the preview strip.
        let split = self.tab_index == 0
            && settings.show_preview
            && compactness != Compactness::Ultracompact
            && area.width >= SPLIT_MIN_WIDTH;
        let preview_height = if split {
            border_size
        } else {
            // Less a column for the scrollbar, without a border to draw it on.
            let width = area.width.saturating_sub(2 + 2 * border_size + 1 - border_size);
            let height =
                self.calc_preview_height(settings, compactness, border_size, width.into(), theme);
            // An automatic height only grows: were it to follow each session's text (or shrink
            // while one is read), the list would jump as the selection moves.
            if settings.show_preview
                && self.tab_index == 0
                && settings.preview.strategy == PreviewStrategy::Auto
            {
                self.strip_height = self.strip_height.max(height);
                self.strip_height
            } else {
                height
            }
        };

        let show_help = settings.show_help && (compactness == Compactness::Full || area.height > 1);
        let show_tabs = settings.show_tabs && compactness != Compactness::Ultracompact;
        let status = self.status_line();
        let status_height = u16::from(status.is_some());
        let help_h = u16::from(show_help);
        let tabs_h = u16::from(show_tabs);

        let constraints: [Constraint; 6] = if invert {
            [
                Constraint::Length(1 + border_size),
                Constraint::Min(1),
                Constraint::Length(preview_height),
                Constraint::Length(tabs_h),
                Constraint::Length(help_h),
                Constraint::Length(status_height),
            ]
        } else if compactness == Compactness::Ultracompact {
            [
                Constraint::Length(help_h),
                Constraint::Length(0),
                Constraint::Min(1),
                Constraint::Length(0),
                Constraint::Length(0),
                Constraint::Length(status_height),
            ]
        } else {
            [
                Constraint::Length(help_h),
                Constraint::Length(tabs_h),
                Constraint::Min(1),
                Constraint::Length(1 + border_size),
                Constraint::Length(preview_height),
                Constraint::Length(status_height),
            ]
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .horizontal_margin(1)
            .constraints(constraints)
            .split(area);

        let (input_chunk, list_chunk, preview_chunk, tabs_chunk, header_chunk) = if invert {
            (chunks[0], chunks[1], chunks[2], chunks[3], chunks[4])
        } else {
            (chunks[3], chunks[2], chunks[4], chunks[1], chunks[0])
        };
        let status_chunk = chunks[5];

        if show_tabs {
            let titles: Vec<Line> = TAB_TITLES.iter().copied().map(Line::from).collect();
            let tabs = Tabs::new(titles)
                .block(Block::default().borders(Borders::NONE))
                .select(self.tab_index)
                .style(Style::default())
                .highlight_style(style(theme, Meaning::Important));
            f.render_widget(tabs, tabs_chunk);
        }

        let st = StyleState {
            compactness,
            invert,
            inner_width: input_chunk.width.into(),
        };

        let header_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(16), Constraint::Min(0), Constraint::Length(16)])
            .split(header_chunk);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("Atuin v{VERSION}"),
                style(theme, Meaning::Base).add_modifier(Modifier::BOLD),
            )),
            header_chunks[0],
        );
        let help = self.build_help(settings, header_chunks[1].width.into(), theme);
        f.render_widget(help, header_chunks[1]);
        f.render_widget(self.build_stats(theme), header_chunks[2]);

        if let Some((message, meaning)) = &status {
            f.render_widget(
                Paragraph::new(Span::styled(
                    message.clone(),
                    style(theme, *meaning).add_modifier(Modifier::BOLD),
                )),
                status_chunk,
            );
        }

        // Two lines a row, but one in the ultracompact picker; grouped by day while newest first.
        let shape = RowShape {
            two_line: compactness != Compactness::Ultracompact,
            by_day: self.parsed_query().text.trim().is_empty(),
        };

        let indicator = match compactness {
            Compactness::Ultracompact => {
                format!("{}> ", self.mode_label().chars().next().unwrap_or(' '))
            }
            _ => " > ".to_owned(),
        };

        if self.tab_index == 1 {
            self.draw_inspect(f, list_chunk, st, settings, theme);
            let bold = Style::default().add_modifier(Modifier::BOLD);
            let guide = if self.expanded_children().is_some() {
                Line::from(vec![
                    Span::styled("<↑/↓>", bold),
                    Span::raw(": move  "),
                    Span::styled("<c>", bold),
                    Span::raw("/"),
                    Span::styled("<esc>", bold),
                    Span::raw(": collapse"),
                ])
            } else {
                Line::from(vec![
                    Span::styled("<esc>", bold),
                    Span::raw(": back  "),
                    Span::styled("<enter>", bold),
                    Span::raw(if settings.enter_accept {
                        ": resume  "
                    } else {
                        ": edit  "
                    }),
                    Span::styled("<tab>", bold),
                    Span::raw(": edit  "),
                    Span::styled("<ctrl-y>", bold),
                    Span::raw(": copy"),
                ])
            };
            let guide = Paragraph::new(guide).style(style(theme, Meaning::Annotation));
            f.render_widget(input_block(guide, st), input_chunk);
            return;
        }

        let block = match compactness {
            Compactness::Full if invert => Some(
                Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded)
                    .title(format!("{:─>width$}", "", width = st.inner_width.saturating_sub(2))),
            ),
            Compactness::Full => Some(
                Block::default()
                    .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded),
            ),
            _ => None,
        };
        let inner = block.as_ref().map_or(list_chunk, |b| b.inner(list_chunk));
        if let Some(block) = block {
            f.render_widget(block, list_chunk);
        }
        let (list_area, divider, pane) = if split {
            let [list_area, divider, pane] = Layout::horizontal([
                Constraint::Fill(3),
                Constraint::Length(1),
                Constraint::Fill(2),
            ])
            .areas(inner);
            (list_area, Some(divider), Some(pane))
        } else {
            (inner, None, None)
        };

        let cells = row_layout(list_area.width, shape);
        // Inside the border around the row the chooser is open under.
        let boxed_cells = row_layout(list_area.width.saturating_sub(2), shape);
        // The chooser opens under the row it's for, while that's the one selected.
        let chooser_lines = self
            .chooser
            .as_ref()
            .filter(|c| self.selected().is_some_and(|r| r.handle == c.row.handle))
            .and_then(|c| {
                let budget = usize::from(list_area.width.saturating_sub(shape.title_x() + 2));
                let (lines, _) = self.chooser_lines(budget, theme)?;
                // Its selected line, past the rule when it's one of the other agents.
                let past_rule = !c.targets.is_empty() && c.selected >= c.len() - c.targets.len();
                Some((lines, c.selected + usize::from(past_rule)))
            });
        self.chooser_inline = chooser_lines.is_some();
        let list = SessionList {
            rows: &self.results,
            block: None,
            inverted: invert,
            alternate_highlight: self.keymap_mode == KeymapMode::VimNormal,
            now: (self.now)(),
            tz: settings.timezone.0,
            indicator: &indicator,
            theme,
            cells: &cells,
            shape,
            here: &self.context.host_id,
            chooser: chooser_lines.as_ref().map(|(lines, at)| (lines.as_slice(), *at)),
            boxed_cells: &boxed_cells,
        };
        f.render_stateful_widget(list, list_area, &mut self.list);
        self.list_anchor = self.anchor(list_area, shape);

        // A scrollbar on the right border (or the divider) once the list overflows.
        let visible = usize::from(list_area.height);
        let track = divider.unwrap_or(Rect {
            x: list_chunk.right().saturating_sub(1),
            y: list_area.y,
            width: 1,
            height: list_area.height,
        });
        if let Some(divider) = divider {
            let line = style(theme, Meaning::Annotation);
            for y in divider.top()..divider.bottom() {
                f.buffer_mut()[(divider.x, y)].set_symbol("│").set_style(line);
            }
        }
        let lines = self.list.lines;
        if lines > visible && (divider.is_some() || compactness == Compactness::Full) {
            let top = if invert {
                self.list.offset
            } else {
                lines.saturating_sub(self.list.offset + visible)
            };
            let mut state = ScrollbarState::new(lines.saturating_sub(visible))
                .position(top)
                .viewport_content_length(visible);
            let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .style(style(theme, Meaning::Annotation));
            f.render_stateful_widget(bar, track, &mut state);
        }

        if let Some(pane) = pane {
            self.draw_side(f, pane, settings.timezone.0, theme);
        }

        if compactness == Compactness::Ultracompact {
            return;
        }

        // Line the query up with the titles, as the history search lines it up with the command.
        let prefix_width = shape.title_x();
        f.render_widget(self.build_input(st, prefix_width, theme), input_chunk);

        // The text, and a column for the scrollbar: the right border, or one kept for it.
        let strip = Rect {
            x: preview_chunk.x + border_size,
            y: preview_chunk.y + border_size,
            width: preview_chunk.width.saturating_sub(2 * border_size + 1 - border_size),
            height: preview_chunk.height.saturating_sub(2 * border_size),
        };
        // Beside the pane (or with no room), the strip is only the box's bottom border.
        let (lines, scrolled) = if split || strip.height == 0 {
            (Vec::new(), None)
        } else {
            self.strip_lines(usize::from(strip.width), usize::from(strip.height), theme)
        };
        let preview = match compactness {
            Compactness::Full => Paragraph::new(Text::from(lines)).block(
                Block::default()
                    .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                    .border_type(BorderType::Rounded)
                    .title(format!(
                        "{:─>width$}",
                        "",
                        width = usize::from(preview_chunk.width).saturating_sub(2)
                    )),
            ),
            _ => Paragraph::new(Text::from(lines)).style(style(theme, Meaning::Annotation)),
        };
        f.render_widget(preview, preview_chunk);
        if let Some((session, top, body)) = scrolled {
            let top = u16::try_from(top).unwrap_or(0);
            let height = strip.height.saturating_sub(top);
            let track = Rect {
                x: preview_chunk.right().saturating_sub(1),
                y: strip.y + top,
                width: 1,
                height,
            };
            draw_scrollbar(f, track, &body, usize::from(height), theme);
            self.drawn(Pane::Strip, session, preview_chunk, usize::from(height), &body);
        }

        // Join the divider to the box's top and bottom borders.
        if let Some(divider) = divider
            && compactness == Compactness::Full
        {
            // (On a terminal too short for the box, there may be no border to join.)
            let buf = f.buffer_mut();
            if let Some(top) = buf.cell_mut((divider.x, list_chunk.y)) {
                let border = top.style();
                top.set_symbol("┬").set_style(border);
                let below = list_chunk.bottom();
                if below < area.bottom()
                    && let Some(bottom) = buf.cell_mut((divider.x, below))
                {
                    bottom.set_symbol("┴").set_style(border);
                }
            }
        }

        let before_cursor = self.input.substring().width();
        let cursor_offset = border_size;
        f.set_cursor_position((
            input_chunk
                .x
                .saturating_add(u16::try_from(before_cursor).unwrap_or(u16::MAX))
                .saturating_add(prefix_width)
                .saturating_add(cursor_offset),
            input_chunk.y.saturating_add(cursor_offset),
        ));
    }

    /// What the keys do to the selected session, as many as fit in `width` columns: `enter
    /// resume in Claude Code · tab edit command · ctrl-y copy · ctrl-o inspect · esc exit`.
    /// (Forking is in the chooser enter opens.) While the chooser is open, what they do there:
    /// `enter resume · tab edit command · esc back`.
    fn build_help(&self, settings: &Settings, width: usize, theme: &Theme) -> Paragraph<'static> {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut actions: Vec<(&str, String)> = Vec::new();
        if let Some(chooser) = &self.chooser {
            if chooser.action == Pending::Resume {
                actions.push(("enter", "resume".to_owned()));
                actions.push(("tab", "edit command".to_owned()));
            } else {
                actions.push(("enter", "edit command".to_owned()));
            }
            actions.push(("esc", "back".to_owned()));
        } else if self.tab_index == 0
            && let Some(row) = self.selected()
        {
            let agent = harness_label(row.handle.harness);
            let enter = if settings.enter_accept {
                match self.plans.get(&row.handle) {
                    Some(Err(_)) => "continue in another agent".to_owned(),
                    Some(Ok(resume)) if resume.restore.is_some() => {
                        format!("restore and resume in {agent}")
                    }
                    _ => format!("resume in {agent}"),
                }
            } else {
                "edit command".to_owned()
            };
            actions.push(("enter", enter));
            if settings.enter_accept {
                actions.push(("tab", "edit command".to_owned()));
            }
            actions.push(("ctrl-y", "copy".to_owned()));
            actions.push(("ctrl-o", "inspect".to_owned()));
            actions.push(("esc", "exit".to_owned()));
        } else {
            actions.push(("esc", "exit".to_owned()));
        }

        // Drop the last actions (before exit) until the rest fit.
        let len = |a: &[(&str, String)]| {
            a.iter().map(|(k, l)| k.width() + 1 + l.width()).sum::<usize>() + 3 * (a.len() - 1)
        };
        while actions.len() > 1 && len(&actions) > width {
            actions.remove(actions.len() - 2);
        }
        let mut spans = Vec::new();
        for (i, (key, label)) in actions.into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" · "));
            }
            spans.push(Span::styled(key.to_owned(), bold));
            spans.push(Span::raw(format!(" {label}")));
        }
        Paragraph::new(Line::from(spans))
            .style(style(theme, Meaning::Annotation))
            .alignment(Alignment::Center)
    }

    fn build_stats(&self, theme: &Theme) -> Paragraph<'static> {
        let text = match self.result_count() {
            None => String::new(),
            Some(n) if n == "1" => "1 session".to_owned(),
            Some(n) => format!("{n} sessions"),
        };
        Paragraph::new(text).style(style(theme, Meaning::Annotation)).alignment(Alignment::Right)
    }

    /// What the list holds, for the header and the mode prefix: `105`, or `500+` when the search
    /// stopped at its limit. `None` before the first results.
    fn result_count(&self) -> Option<String> {
        let n = self.results.len();
        (self.applied != 0).then(|| {
            if n >= SEARCH_LIMIT {
                format!("{SEARCH_LIMIT}+")
            } else {
                n.to_string()
            }
        })
    }

    fn build_input(&self, st: StyleState, prefix_width: u16, theme: &Theme) -> Paragraph<'static> {
        let muted = style(theme, Meaning::Annotation);
        // The prompt ends where the titles start.
        let width = usize::from(prefix_width);
        let mut spans = vec![Span::styled(format!("{:>width$}", "› "), muted)];

        // Filter tokens render as chips; the rest is plain text.
        let input = self.input.as_str();
        let parsed = self.parsed_query();
        let mut at = 0;
        for token in &parsed.tokens {
            if token.range.start > at {
                spans.push(Span::raw(input[at..token.range.start].to_owned()));
            }
            let chip = match (token.state, token.kind) {
                (TokenState::Invalid, _) => {
                    style(theme, Meaning::AlertError).add_modifier(Modifier::UNDERLINED)
                }
                (TokenState::Pending, _) => {
                    style(theme, Meaning::Annotation).add_modifier(Modifier::REVERSED)
                }
                (TokenState::Valid, TokenKind::Harness) => {
                    style(theme, Meaning::Important).add_modifier(Modifier::REVERSED)
                }
                (TokenState::Valid, _) => {
                    style(theme, Meaning::Guidance).add_modifier(Modifier::REVERSED)
                }
            };
            spans.push(Span::styled(input[token.range.clone()].to_owned(), chip));
            at = token.range.end;
        }
        if at < input.len() {
            spans.push(Span::raw(input[at..].to_owned()));
        }

        // The scopes, at the right while the query leaves room: the one listed, and the others.
        let scopes = self.scope_spans(theme);
        let scopes_width: usize = scopes.iter().map(|s| s.content.width()).sum();
        let borders = if st.compactness == Compactness::Full {
            2
        } else {
            0
        };
        let room = st.inner_width.saturating_sub(borders);
        let used = usize::from(prefix_width) + input.width();
        if let Some(gap) = room.checked_sub(used + scopes_width).filter(|g| *g >= 2) {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.extend(scopes);
        }

        input_block(Paragraph::new(Line::from(spans)), st)
    }

    /// The scopes the list can show, in the order ctrl-r goes through them: `ctrl-r  all  repo
    /// branch  dir  host`, the one listed highlighted. Those that can't apply here (a repository
    /// or branch outside one) are left out.
    fn scope_spans(&self, theme: &Theme) -> Vec<Span<'static>> {
        let muted = style(theme, Meaning::Annotation);
        let current =
            style(theme, Meaning::Important).add_modifier(Modifier::REVERSED | Modifier::BOLD);
        let mut spans = vec![Span::styled("ctrl-r ", muted)];
        for mode in FilterMode::CYCLE.into_iter().filter(|m| self.mode_available(*m)) {
            let label = format!(" {} ", scope_label(mode));
            spans.push(if mode == self.mode {
                Span::styled(label, current)
            } else {
                Span::styled(label, muted)
            });
        }
        spans
    }

    #[allow(clippy::too_many_lines)]
    fn draw_inspect(
        &mut self,
        f: &mut Frame,
        chunk: Rect,
        st: StyleState,
        settings: &Settings,
        theme: &Theme,
    ) {
        let block = match st.compactness {
            Compactness::Full if st.invert => Block::default()
                .borders(Borders::LEFT | Borders::RIGHT)
                .border_type(BorderType::Rounded)
                .title(format!("{:─>width$}", "", width = st.inner_width.saturating_sub(2))),
            Compactness::Full => Block::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .border_type(BorderType::Rounded),
            _ => Block::default(),
        };
        let inner = block.inner(chunk);
        f.render_widget(block, chunk);

        let Some(row) = self.selected().cloned() else {
            f.render_widget(
                Paragraph::new("Nothing to inspect").alignment(Alignment::Center),
                inner,
            );
            return;
        };

        let now = (self.now)();
        let tz = settings.timezone.0;
        let key = style(theme, Meaning::Annotation);
        let base = style(theme, Meaning::Base);
        let width = usize::from(inner.width).saturating_sub(11);
        let field = |name: &'static str, spans: Vec<Span<'static>>| {
            let mut line = vec![Span::styled(format!(" {name:<10}"), key)];
            line.extend(spans);
            Line::from(line)
        };
        let text = |s: String| vec![Span::styled(s, base)];
        let when = |ts: OffsetDateTime| format_when(ts, now, tz);

        let mut lines = vec![
            field("Session", vec![
                Span::styled(row.handle.session.to_string(), base.add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(
                    harness_label(row.handle.harness),
                    harness_style(theme, row.handle.harness),
                ),
            ]),
            field("Atuin id", text(row.atuin_id.to_string())),
            field("Title", highlighted_line(&row.title.text, &[], width, base, base)),
            field("Host", vec![
                Span::styled(row.host_id.clone(), base),
                Span::styled(
                    if row.host_id == self.context.host_id {
                        "  (this machine)"
                    } else {
                        "  (another machine)"
                    },
                    key,
                ),
            ]),
            field(
                "Directory",
                text(row.cwd.as_deref().map(|p| p.display().to_string()).unwrap_or_default()),
            ),
            field("Branch", text(row.branch.clone().unwrap_or_default())),
            field("Model", text(row.model.clone().unwrap_or_default())),
            field("Started", text(when(row.started_at))),
            field("Updated", {
                let mut spans = text(when(row.updated_at));
                if is_live(now, &row) {
                    spans.push(Span::styled("  ● live", style(theme, Meaning::AlertInfo)));
                }
                spans
            }),
            field("Messages", text(row.messages.to_string())),
        ];
        if let Some(t) = panel::token_breakdown(&row.usage) {
            lines.push(field("Tokens", text(t)));
        }
        lines.push(match self.plans.get(&row.handle) {
            None => field("Resume", vec![Span::styled("…", key)]),
            Some(Ok(resume)) => field("Resume", vec![
                Span::styled(
                    shell_line(&resume.plan),
                    style(theme, Meaning::Important).add_modifier(Modifier::BOLD),
                ),
                // Its transcript is written from the synced messages first.
                Span::styled(
                    if resume.restore.is_some() {
                        "  from sync"
                    } else {
                        ""
                    },
                    key,
                ),
            ]),
            Some(Err(why)) => field("Resume", vec![Span::styled(
                format!("not resumable: {why}"),
                style(theme, Meaning::AlertError),
            )]),
        });

        // The forks, once read (a row with only subagents grouped under it has none).
        if let Some(forks) = self.children.get(&row.handle).filter(|c| !c.is_empty()).cloned() {
            let left = usize::from(inner.height).saturating_sub(lines.len());
            lines.extend(self.children_lines(&row, &forks, left, inner.width, tz, theme));
        }

        // The conversation, in whatever room is left, scrolling. Its scrollbar goes on the right
        // border, or in a column kept for it.
        let top = u16::try_from(lines.len()).unwrap_or(u16::MAX).min(inner.height);
        let left = usize::from(inner.height.saturating_sub(top));
        let border = u16::from(st.compactness == Compactness::Full);
        let width = usize::from((inner.width + border).saturating_sub(1));
        let want = self.scrolls[Pane::Inspect as usize].offset_for(&row.handle);
        let body = match self.reader_body(Pane::Inspect, &row, width, 1, left, theme) {
            Some(body) => body,
            None => scroll_body(
                want,
                left,
                || self.conversation(&row, width, left, 1, theme),
                |limit| self.conversation_document(&row, width, 1, limit, theme),
            ),
        };
        lines.extend(body.lines.iter().cloned());
        f.render_widget(Paragraph::new(Text::from(lines)), inner);

        let area = Rect {
            y: inner.y + top,
            height: inner.height.saturating_sub(top),
            ..inner
        };
        let track = Rect {
            x: (inner.right() + border).saturating_sub(1),
            width: 1,
            ..area
        };
        draw_scrollbar(f, track, &body, left, theme);
        self.drawn(Pane::Inspect, row.handle.clone(), area, left, &body);
    }

    /// The detail pane beside the list, in `pane`: the session previewed (see
    /// [`State::preview_row`]), its conversation scrolling under what it is, with a scrollbar in
    /// the pane's right margin.
    fn draw_side(&mut self, f: &mut Frame, pane: Rect, tz: UtcOffset, theme: &Theme) {
        let text = pane.inner(ratatui::layout::Margin::new(1, 0));
        let Some(row) = self.preview_row().cloned() else {
            return;
        };
        let width = usize::from(text.width);
        let mut lines = self.detail_header(&row, width, tz, theme);
        let top = u16::try_from(lines.len()).unwrap_or(u16::MAX).min(text.height);
        let left = usize::from(text.height.saturating_sub(top));
        if !self.previews.contains_key(&row.handle) {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("…", style(theme, Meaning::Annotation))));
            // Already wrapped (markdown keeps its indents, which the paragraph's wrap would trim).
            f.render_widget(Paragraph::new(Text::from(lines)), text);
            return;
        }
        let want = self.scrolls[Pane::Side as usize].offset_for(&row.handle);
        let body = match self.reader_body(Pane::Side, &row, width, 0, left, theme) {
            Some(body) => body,
            None => scroll_body(
                want,
                left,
                || self.conversation(&row, width, left, 0, theme),
                |limit| self.conversation_document(&row, width, 0, limit, theme),
            ),
        };
        lines.extend(body.lines.iter().cloned());
        f.render_widget(Paragraph::new(Text::from(lines)), text);

        let track = Rect {
            x: pane.right().saturating_sub(1),
            y: text.y + top,
            width: 1,
            height: text.height.saturating_sub(top),
        };
        draw_scrollbar(f, track, &body, left, theme);
        self.drawn(Pane::Side, row.handle, pane, left, &body);
    }

    /// Inspect's list of the forks grouped under `row`, in at most `room` lines: a blank line, a
    /// heading saying how many, and the tree. Collapsed, it shows the first few with a line
    /// saying how many more (`c` expands it), leaving the rest of the room to the conversation;
    /// expanded, it takes most of the room, scrolls, and has a cursor.
    fn children_lines(
        &mut self,
        row: &SessionRow,
        children: &[SessionRow],
        room: usize,
        width: u16,
        tz: UtcOffset,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let key = style(theme, Meaning::Annotation);
        let heading = panel::forks(Some(children)).unwrap_or_default();
        let mut lines = vec![Line::default()];
        let tree =
            panel::tree_lines(&row.handle, children, (self.now)(), tz, usize::from(width), theme);
        let room = room.saturating_sub(2);
        let expanded = self.expanded_children().is_some();
        let mut heading =
            vec![Span::styled(format!(" {heading}"), key.add_modifier(Modifier::BOLD))];

        let shown: Vec<Line<'static>> =
            if let Some(view) = self.children_view.as_mut().filter(|_| expanded) {
                // Most of the room, leaving the conversation a few lines.
                let height = tree
                    .len()
                    .min(room.saturating_sub(INSPECT_CONVERSATION_MIN).max(CHILDREN_COLLAPSED));
                view.cursor = view.cursor.min(tree.len().saturating_sub(1));
                view.height = height.max(1);
                if view.cursor < view.offset {
                    view.offset = view.cursor;
                } else if view.cursor >= view.offset + view.height {
                    view.offset = view.cursor + 1 - view.height;
                }
                view.offset = view.offset.min(tree.len().saturating_sub(view.height));
                if tree.len() > height {
                    heading.push(Span::styled(
                        format!("  {}–{} of {}", view.offset + 1, view.offset + height, tree.len()),
                        key,
                    ));
                }
                let cursor = view.cursor;
                tree.into_iter()
                    .enumerate()
                    .skip(view.offset)
                    .take(height)
                    .map(|(i, line)| {
                        if i == cursor {
                            line.patch_style(Style::default().add_modifier(Modifier::REVERSED))
                        } else {
                            line
                        }
                    })
                    .collect()
            } else {
                // A few, with a line for the rest, keeping at least half the room for the
                // conversation.
                let fit = (room / 2).saturating_sub(1).clamp(1, CHILDREN_COLLAPSED);
                let n = if tree.len() <= fit + 1 {
                    tree.len().min(room)
                } else {
                    fit
                };
                let more = tree.len().saturating_sub(n);
                let mut shown: Vec<Line<'static>> = tree.into_iter().take(n).collect();
                if more > 0 {
                    shown.push(Line::from(Span::styled(
                        format!("   … and {more} more (c to expand)"),
                        key,
                    )));
                }
                shown
            };
        lines.push(Line::from(heading));
        lines.extend(shown);
        lines
    }
}

/// How many children Inspect shows before `… and N more`.
const CHILDREN_COLLAPSED: usize = 3;
/// The lines Inspect keeps for the conversation while the children list is expanded.
const INSPECT_CONVERSATION_MIN: usize = 4;

/// A full date and time, with what it doesn't already say beside it: `(12m ago)`,
/// `(yesterday)`, `(Monday)`.
/// A scope's name in the input's list of them.
pub fn scope_label(mode: FilterMode) -> &'static str {
    match mode {
        FilterMode::Global => "all",
        FilterMode::Workspace => "repo",
        FilterMode::Branch => "branch",
        FilterMode::Directory => "dir",
        FilterMode::Host => "host",
    }
}

fn format_when(ts: OffsetDateTime, now: OffsetDateTime, tz: UtcOffset) -> String {
    let at = ts.to_offset(tz).display().ymd_hm();
    match clock::beside_date(now, ts, tz) {
        Some(when) => format!("{at}  ({when})"),
        None => at.to_string(),
    }
}

/// The input box's borders, as in the history search.
fn input_block(p: Paragraph<'static>, st: StyleState) -> Paragraph<'static> {
    match st.compactness {
        Compactness::Full if st.invert => p.block(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP)
                .border_type(BorderType::Rounded),
        ),
        Compactness::Full => p.block(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT)
                .border_type(BorderType::Rounded)
                .title(format!("{:─>width$}", "", width = st.inner_width.saturating_sub(2))),
        ),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use Column::{Harness, Messages, Time, Title};
    use rstest::rstest;

    use super::*;

    const ONE_LINE: RowShape = RowShape {
        two_line: false,
        by_day: false,
    };
    const TWO_LINE: RowShape = RowShape {
        two_line: true,
        by_day: false,
    };
    const BY_DAY: RowShape = RowShape {
        two_line: true,
        by_day: true,
    };

    /// The title takes the rest of the row, whatever the width: 80 columns is 76 inside the box,
    /// and a split 120 leaves the list 69. Two-line rows give the time's and the badge's room to
    /// the title.
    #[rstest]
    #[case::wide(196, 174)]
    #[case::full_80(76, 54)]
    #[case::split_120(69, 47)]
    fn the_title_takes_the_rest(#[case] width: u16, #[case] title: u16) {
        assert_eq!(row_layout(width, ONE_LINE), [
            (Time, 10),
            (Harness, 2),
            (Title, title),
            (Messages, 4)
        ]);
        assert_eq!(row_layout(width, TWO_LINE), [(Title, title + 14), (Messages, 4)]);
        assert_eq!(row_layout(width, BY_DAY), [(Title, title + 14), (Messages, 4)]);
    }

    /// At every width the row fills it exactly, and the message count goes only when the title
    /// would otherwise be under [`TITLE_MIN`].
    #[rstest]
    fn every_width_fills_the_row_title_first(
        #[values(ONE_LINE, TWO_LINE, BY_DAY)] shape: RowShape,
    ) {
        for width in 30..=200u16 {
            let layout = row_layout(width, shape);
            let used: u16 = layout.iter().map(|(_, w)| w).sum::<u16>()
                + u16::try_from(layout.len() - 1).unwrap()
                + 3;
            let (_, title) = layout.iter().find(|(c, _)| *c == Title).copied().unwrap();
            if title > 0 {
                assert_eq!(used, width, "{width}: {layout:?}");
            }
            let counted = layout.iter().any(|(c, _)| *c == Messages);
            let gain = Messages.width() + 1;
            if counted {
                assert!(title >= TITLE_MIN, "{width}: {layout:?}");
            } else {
                assert!(title < TITLE_MIN + gain, "{width}: {layout:?}");
            }
        }
    }

    /// A day's heading reads over its sessions, and a row's title over its place, whichever way
    /// up the list is.
    #[rstest]
    fn headings_and_titles_read_top_down_either_way_up() {
        use ListLine::{Heading, Place, Title as T};
        let rows: Vec<SessionRow> = (0..3)
            .map(|i| super::super::fake::row(HarnessKind::ClaudeCode, &format!("s{i}"), "t"))
            .collect();
        // The first two on one day, the third on another.
        let day = |r: &SessionRow| {
            let d = time::macros::date!(2026 - 10 - 08);
            if r.handle.session.as_ref() == "s2" {
                d.previous_day().unwrap()
            } else {
                d
            }
        };
        let (down, starts) = list_lines(&rows, BY_DAY, true, None, day);
        assert_eq!(down, [Heading(0), T(0), Place(0), T(1), Place(1), Heading(2), T(2), Place(2)]);
        assert_eq!(starts, [1, 3, 6]);
        // From an input under the list, outward is up the screen.
        let (up, starts) = list_lines(&rows, BY_DAY, false, None, day);
        assert_eq!(up, [Place(0), T(0), Place(1), T(1), Heading(0), Place(2), T(2), Heading(2)]);
        assert_eq!(starts, [0, 2, 5]);
        // Ungrouped, one line a row.
        let (lines, _) = list_lines(&rows, ONE_LINE, false, None, day);
        assert_eq!(lines, [T(0), T(1), T(2)]);
    }

    #[rstest]
    #[case(9_999, "9999")]
    #[case(12_345, "12k")]
    fn message_counts_fit_their_column(#[case] n: u64, #[case] want: &str) {
        assert_eq!(message_count(n), want);
        assert!(want.len() <= usize::from(Messages.width()));
    }
}
