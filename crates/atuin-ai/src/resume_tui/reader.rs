//! The reader: a session's whole conversation in the detail pane and Inspect, scrolling, opened at
//! the search's match.
//!
//! It renders a transcript's entries as far as the view needs and keeps them between frames
//! ([`Rendered`]), so scrolling a long session renders each entry once.

use std::sync::Arc;

use atuin_client::ai_session::HarnessSession;
use atuin_client::theme::{Meaning, Theme};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::render::{Body, harness_style, style};
use super::source::{SessionRow, TranscriptEntry, harness_label};
use super::state::{Pane, State};
use super::{markdown, title};

/// What the reader rendered, and for what: rendered again from the start when any of it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    session: HarnessSession,
    /// The transcript's version (see [`State::transcripts`]).
    version: u64,
    width: usize,
    indent: usize,
    /// The query text, whose words are highlighted.
    query: String,
}

/// The reader's lines rendered so far.
#[derive(Debug)]
pub struct Rendered {
    key: Key,
    lines: Vec<Line<'static>>,
    /// The line each entry rendered so far starts on: its heading, when it has one.
    starts: Vec<usize>,
}

/// Who an entry is from: a heading goes over each change of speaker.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Speaker {
    User,
    Agent,
    Summary,
}

fn speaker(entry: &TranscriptEntry) -> Speaker {
    match entry {
        TranscriptEntry::Prompt(_) => Speaker::User,
        TranscriptEntry::Reply(_) | TranscriptEntry::Tools(_) => Speaker::Agent,
        TranscriptEntry::Summary(_) => Speaker::Summary,
    }
}

/// How much the text sits in from its heading.
const TEXT_INDENT: usize = 2;

/// A run of tool calls on one line: `Read ×3 · Edit · Bash ×2`.
fn tools_line(tools: &[(String, usize)]) -> String {
    tools
        .iter()
        .map(|(name, n)| {
            if *n > 1 {
                format!("{name} ×{n}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

impl Rendered {
    fn new(key: Key) -> Self {
        Self {
            key,
            lines: Vec::new(),
            starts: Vec::new(),
        }
    }

    /// Render the next entry of `entries`, after those rendered already.
    fn push(&mut self, entries: &[TranscriptEntry], row: &SessionRow, theme: &Theme) {
        let i = self.starts.len();
        let Some(entry) = entries.get(i) else {
            return;
        };
        let Key {
            width,
            indent,
            query,
            ..
        } = &self.key;
        let pad = " ".repeat(*indent);
        let text_pad = " ".repeat(indent + TEXT_INDENT);
        let text_width = width.saturating_sub(indent + TEXT_INDENT).max(1);
        let base = style(theme, Meaning::Base);
        let muted = style(theme, Meaning::Annotation);

        self.lines.push(Line::default());
        self.starts.push(self.lines.len());
        let who = speaker(entry);
        if i == 0 || speaker(&entries[i - 1]) != who {
            let heading = match who {
                Speaker::User => Span::styled("You", base.add_modifier(Modifier::BOLD)),
                Speaker::Agent => Span::styled(
                    harness_label(row.handle.harness),
                    harness_style(theme, row.handle.harness).add_modifier(Modifier::BOLD),
                ),
                Speaker::Summary => Span::styled("Summary", muted.add_modifier(Modifier::BOLD)),
            };
            self.lines.push(Line::from(vec![Span::raw(pad), heading]));
        }

        let body = match entry {
            TranscriptEntry::Tools(tools) => markdown::wrap_plain(
                &[Span::styled(format!("⚙ {}", tools_line(tools)), muted)],
                text_width,
                0,
                muted,
            ),
            TranscriptEntry::Prompt(text)
            | TranscriptEntry::Reply(text)
            | TranscriptEntry::Summary(text) => {
                let highlights = if query.is_empty() {
                    Vec::new()
                } else {
                    title::highlights(text, query)
                };
                let text_style: Style = if who == Speaker::Summary {
                    muted
                } else {
                    base
                };
                let opts = markdown::Opts {
                    width: text_width,
                    max_lines: 0,
                    spacing: true,
                    urls: true,
                };
                markdown::render(text, &highlights, opts, &markdown::Styles::new(theme, text_style))
            }
        };
        for line in body {
            let mut spans = vec![Span::raw(text_pad.clone())];
            spans.extend(line.spans);
            self.lines.push(Line::from(spans));
        }
    }
}

/// An entry's text: none for tool calls.
fn text(entry: &TranscriptEntry) -> Option<&str> {
    match entry {
        TranscriptEntry::Prompt(t) | TranscriptEntry::Reply(t) | TranscriptEntry::Summary(t) => {
            Some(t.as_str())
        }
        TranscriptEntry::Tools(_) => None,
    }
}

/// The entry the reader opens at: the one holding the search's match, else the first with a word
/// of the query in it, else the first.
fn opening_entry(entries: &[TranscriptEntry], row: &SessionRow, query: &str) -> usize {
    if query.is_empty() {
        return 0;
    }
    let matched = row.matched.as_ref().map(|m| m.text.as_str()).filter(|m| !m.is_empty());
    matched
        .and_then(|m| entries.iter().position(|e| text(e).is_some_and(|t| t.contains(m))))
        .or_else(|| {
            entries
                .iter()
                .position(|e| text(e).is_some_and(|t| !title::highlights(t, query).is_empty()))
        })
        .unwrap_or(0)
}

impl State {
    /// The reader for `row` in `pane`: `height` lines of its conversation in `width` columns,
    /// indented by `indent`, from where the pane was scrolled to, or, the first time the pane
    /// shows `row`, from the search's match. `None` until its transcript is read (or when it has
    /// none), for the preview to stand in.
    pub(super) fn reader_body(
        &mut self,
        pane: Pane,
        row: &SessionRow,
        width: usize,
        indent: usize,
        height: usize,
        theme: &Theme,
    ) -> Option<Body> {
        let (version, entries) = self.transcripts.get(&row.handle)?;
        let (version, entries): (u64, Arc<Vec<TranscriptEntry>>) = (*version, Arc::clone(entries));
        if entries.is_empty() {
            return None;
        }
        let query = self.parsed_query().text.trim().to_owned();
        let key = Key {
            session: row.handle.clone(),
            version,
            width,
            indent,
            query,
        };
        let rendered = match &mut self.reader {
            Some(r) if r.key == key => r,
            slot => slot.insert(Rendered::new(key)),
        };

        let scroll = &mut self.scrolls[pane as usize];
        let reading = (row.handle.clone(), rendered.key.query.clone());
        let same = scroll.reading.as_ref() == Some(&reading);
        scroll.reading = Some(reading);
        let want = if same && scroll.session.as_ref() == Some(&row.handle) {
            scroll.offset
        } else {
            let open = opening_entry(&entries, row, &rendered.key.query);
            while rendered.starts.len() <= open {
                rendered.push(&entries, row, theme);
            }
            // The blank line over it stays above the view.
            rendered.starts[open]
        };
        // A screen past what's shown, so the scrollbar knows there's more.
        while rendered.lines.len() < want + 2 * height && rendered.starts.len() < entries.len() {
            rendered.push(&entries, row, theme);
        }

        let len = rendered.lines.len();
        let more = rendered.starts.len() < entries.len();
        let offset = want.min(len.saturating_sub(height));
        Some(Body {
            lines: rendered.lines.iter().skip(offset).take(height).cloned().collect(),
            offset,
            len,
            more,
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::source::TranscriptBuilder;
    use super::*;

    #[rstest]
    fn runs_of_tool_calls_fold_into_one_line() {
        let mut t = TranscriptBuilder::default();
        t.prompt("fix it");
        t.tool("Read");
        t.tool("Read");
        t.tool("Edit");
        t.reply("done");
        t.tool("Bash");
        let entries = t.finish();
        assert_eq!(entries.len(), 4);
        let TranscriptEntry::Tools(tools) = &entries[1] else {
            panic!("{entries:?}");
        };
        assert_eq!(tools_line(tools), "Read ×2 · Edit");
    }
}
