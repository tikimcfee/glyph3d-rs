//! Terminal hyperlink hit-testing (`OSC 8` spans + plain-text URL matching across
//! soft-wrapped rows) and URL scheme validation.
//!
//! Operates against [`ScreenView`] so `wall-core` does not depend on `term-cells`'s
//! internal hyperlink storage layout landing first: [`term_cells::Screen`] implements
//! [`ScreenView`] with `hyperlink_at` returning `None` until `term-cells` carries link
//! spans, at which point that single method forwards to `Screen`.
//!
//! ### Policy
//! - Allowed schemes are strictly `http`, `https`, and `mailto` (case-insensitive).
//!   `file://`, `javascript:`, `data:`, and all other schemes are rejected for both
//!   `OSC 8` and plain-text matches (matching the JS wall, which never allowed `file:`
//!   URL execution from terminal cells).
//! - Explicit `OSC 8` links take precedence over plain-text URL matching on the same
//!   cell; an `OSC 8` link with a disallowed scheme suppresses plain-text fallback on
//!   those cells rather than opening a different hidden URL.

use term_cells::{Screen, CLUSTER_BASE};

/// Kind of terminal link detected under a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// Explicit `OSC 8` hyperlink span attached to the cell(s).
    Osc8,
    /// Plain-text URL matched across a logical (possibly soft-wrapped) line.
    PlainUrl,
}

/// Clickable / underlined link span inside a terminal grid (`TerminalFrame::hovered_link`).
///
/// Coordinates are 0-based inclusive-start, exclusive-end linear grid coordinates
/// `(start_row, start_col)..(end_row, end_col)`, matching `CellLinkHover` in
/// `native/src/wall/cell.rs`. When a link wraps across soft-wrapped rows, `end_row > start_row`
/// and every column on intermediate rows is part of the span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoveredLink {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    pub url: String,
    pub kind: LinkKind,
}

/// Read-only view over a terminal screen's cells, soft-wrap flags, and optional `OSC 8`
/// hyperlinks.
///
/// [`Screen`] implements this trait directly (`hyperlink_at` returning `None` until
/// `term_cells::Screen` exposes link spans). Callers with explicit hyperlink data can
/// implement either [`Self::hyperlink_at`] (per-cell URI; spans are grown across contiguous
/// soft-wrapped cells automatically) or [`Self::hyperlink_span_at`] (direct span lookup).
pub trait ScreenView {
    /// Underlying [`Screen`] carrying cell contents, widths, clusters, and soft-wrap flags.
    fn cells_screen(&self) -> &Screen;

    /// `OSC 8` hyperlink URI attached to 0-based cell `(row, col)`, if any.
    ///
    /// Both halves of a wide character should return the same URI.
    fn hyperlink_at(&self, row: usize, col: usize) -> Option<&str> {
        let _ = (row, col);
        None
    }

    /// Full `OSC 8` hyperlink span covering 0-based cell `(row, col)`, if any.
    ///
    /// Defaults to growing contiguous cells sharing the same URI returned by
    /// [`Self::hyperlink_at`] across soft-wrapped rows. Producers that track explicit
    /// `OSC 8` span boundaries or link IDs can override this directly.
    fn hyperlink_span_at(&self, row: usize, col: usize) -> Option<HoveredLink> {
        osc8_span_from_cell_uri(self.cells_screen(), row, col, |r, c| {
            self.hyperlink_at(r, c)
        })
    }
}

impl ScreenView for Screen {
    fn cells_screen(&self) -> &Screen {
        self
    }

    fn hyperlink_at(&self, row: usize, col: usize) -> Option<&str> {
        let col = u16::try_from(col).ok()?;
        let row = u16::try_from(row).ok()?;
        Screen::hyperlink_at(self, col, row)
    }

    fn hyperlink_span_at(&self, row: usize, col: usize) -> Option<HoveredLink> {
        let col = u16::try_from(col).ok()?;
        let row = u16::try_from(row).ok()?;
        let link = Screen::hyperlink_range_at(self, col, row)?;
        Some(HoveredLink {
            start_row: usize::from(link.start_row),
            start_col: usize::from(link.start_col),
            end_row: usize::from(link.end_row),
            end_col: usize::from(link.end_col),
            url: link.uri.to_owned(),
            kind: LinkKind::Osc8,
        })
    }
}

/// Whether `url` has an allowed external scheme (`http`, `https`, or `mailto`) and
/// contains no whitespace or ASCII control characters.
///
/// Rejects `file://`, `javascript:`, `data:`, `ftp://`, `ssh:`, empty authorities,
/// bare `mailto:`, and malformed addresses.
pub fn is_allowed_url_scheme(url: &str) -> bool {
    if url.is_empty()
        || url
            .chars()
            .any(|c| c.is_whitespace() || (c as u32) <= 0x1F || (c as u32) == 0x7F)
    {
        return false;
    }
    if let Some(rest) = strip_ascii_prefix_ignore_case(url, "https://")
        .or_else(|| strip_ascii_prefix_ignore_case(url, "http://"))
    {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        return !authority.is_empty() && authority.bytes().any(|b| b.is_ascii_alphanumeric());
    }
    if let Some(rest) = strip_ascii_prefix_ignore_case(url, "mailto:") {
        let addr = rest.split('?').next().unwrap_or("");
        let Some((local, domain)) = addr.split_once('@') else {
            return false;
        };
        return !local.is_empty()
            && !domain.is_empty()
            && !domain.contains('@')
            && domain.bytes().any(|b| b.is_ascii_alphanumeric());
    }
    false
}

/// Hit-test cell `(row, col)` (0-based) for a clickable link: checks `OSC 8` spans first,
/// then falls back to plain-text URL matching across the cell's soft-wrapped logical line.
///
/// Returns `None` if `(row, col)` is out of bounds, has no link, or its URI scheme is
/// disallowed by [`is_allowed_url_scheme`].
pub fn link_at_cell(view: &dyn ScreenView, row: usize, col: usize) -> Option<HoveredLink> {
    let screen = view.cells_screen();
    let cols = screen.cols as usize;
    let rows = screen.rows as usize;
    if cols == 0 || rows == 0 || row >= rows || col >= cols {
        return None;
    }
    let col = normalize_wide_col(screen, row, col);

    // 1. Explicit OSC 8 hyperlink takes precedence over plain-text URL matching.
    if let Some(span) = view.hyperlink_span_at(row, col) {
        if !span.url.is_empty() {
            return is_allowed_url_scheme(&span.url).then_some(span);
        }
    }

    // 2. Plain-text URL matched across the logical line (joining soft-wrapped rows).
    plain_url_at_cell(screen, row, col)
}

/// Build an [`HoveredLink`] (`LinkKind::Osc8`) by expanding from `(row, col)` across
/// contiguous cells (crossing soft-wrapped row boundaries) that report the same URI.
pub fn osc8_span_from_cell_uri<'a, F>(
    screen: &Screen,
    row: usize,
    col: usize,
    mut uri_at: F,
) -> Option<HoveredLink>
where
    F: FnMut(usize, usize) -> Option<&'a str>,
{
    let cols = screen.cols as usize;
    let rows = screen.rows as usize;
    if cols == 0 || rows == 0 || row >= rows || col >= cols {
        return None;
    }
    let col = normalize_wide_col(screen, row, col);
    let uri = uri_at(row, col).filter(|s| !s.is_empty())?.to_owned();

    let (first_row, last_row) = logical_line_rows(screen, row);

    // Walk backward within `first_row..=row`.
    let mut start_row = row;
    let mut start_col = col;
    while let Some((pr, pc)) = prev_cell_in_line(screen, first_row, start_row, start_col) {
        let norm_c = normalize_wide_col(screen, pr, pc);
        if uri_at(pr, norm_c) == Some(uri.as_str()) {
            start_row = pr;
            start_col = norm_c;
        } else {
            break;
        }
    }

    // Walk forward within `row..=last_row`.
    let mut end_row = row;
    let mut end_col_incl = col;
    loop {
        let step = cell_width(screen, end_row, end_col_incl).max(1) as usize;
        let Some((nr, nc)) = advance_cell_in_line(screen, last_row, end_row, end_col_incl, step)
        else {
            break;
        };
        let norm_c = normalize_wide_col(screen, nr, nc);
        if uri_at(nr, norm_c) == Some(uri.as_str()) {
            end_row = nr;
            end_col_incl = norm_c;
        } else {
            break;
        }
    }

    let last_w = cell_width(screen, end_row, end_col_incl).max(1) as usize;
    let end_col = (end_col_incl + last_w).min(cols);

    Some(HoveredLink {
        start_row,
        start_col,
        end_row,
        end_col,
        url: uri,
        kind: LinkKind::Osc8,
    })
}

// ── Internal helpers ─────────────────────────────────────────────────────────

#[inline]
fn row_wraps_into_next(screen: &Screen, r: usize) -> bool {
    r + 1 < screen.rows as usize && screen.wrapped.get(r).copied().unwrap_or(false)
}

#[inline]
fn cell_width(screen: &Screen, row: usize, col: usize) -> u8 {
    let cols = screen.cols as usize;
    screen
        .cells
        .get(row * cols + col)
        .map_or(1, |c| c.width)
}

#[inline]
fn normalize_wide_col(screen: &Screen, row: usize, col: usize) -> usize {
    if col > 0 && cell_width(screen, row, col) == 0 {
        col - 1
    } else {
        col
    }
}

fn logical_line_rows(screen: &Screen, row: usize) -> (usize, usize) {
    let mut first = row;
    while first > 0 && row_wraps_into_next(screen, first - 1) {
        first -= 1;
    }
    let mut last = row;
    while row_wraps_into_next(screen, last) {
        last += 1;
    }
    (first, last)
}

fn prev_cell_in_line(
    screen: &Screen,
    first_row: usize,
    row: usize,
    col: usize,
) -> Option<(usize, usize)> {
    let cols = screen.cols as usize;
    if col > 0 {
        Some((row, col - 1))
    } else if row > first_row && cols > 0 {
        Some((row - 1, cols - 1))
    } else {
        None
    }
}

fn advance_cell_in_line(
    screen: &Screen,
    last_row: usize,
    row: usize,
    col: usize,
    step: usize,
) -> Option<(usize, usize)> {
    let cols = screen.cols as usize;
    if col + step < cols {
        Some((row, col + step))
    } else if row < last_row {
        Some((row + 1, 0))
    } else {
        None
    }
}

fn strip_ascii_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let bytes = s.as_bytes();
    let pbytes = prefix.as_bytes();
    if bytes.len() < pbytes.len() {
        return None;
    }
    if bytes[..pbytes.len()].eq_ignore_ascii_case(pbytes) {
        Some(&s[pbytes.len()..])
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug)]
struct CellSpanEntry {
    byte_start: usize,
    byte_end: usize,
    row: usize,
    col: usize,
    width: usize,
}

fn plain_url_at_cell(screen: &Screen, target_row: usize, target_col: usize) -> Option<HoveredLink> {
    let cols = screen.cols as usize;
    let (first_row, last_row) = logical_line_rows(screen, target_row);

    let mut line = String::with_capacity((last_row - first_row + 1) * cols);
    let mut entries: Vec<CellSpanEntry> = Vec::with_capacity((last_row - first_row + 1) * cols);

    for r in first_row..=last_row {
        let row_cells = &screen.cells[r * cols..r * cols + cols];
        for (c, cell) in row_cells.iter().enumerate() {
            if cell.width == 0 {
                continue;
            }
            let byte_start = line.len();
            if cell.content == 0 {
                line.push(' ');
            } else if cell.content >= CLUSTER_BASE {
                if let Some(cluster) = screen.clusters.get((cell.content - CLUSTER_BASE) as usize) {
                    line.push_str(cluster);
                } else {
                    line.push(' ');
                }
            } else if let Some(ch) = char::from_u32(cell.content) {
                line.push(ch);
            } else {
                line.push(' ');
            }
            let byte_end = line.len();
            entries.push(CellSpanEntry {
                byte_start,
                byte_end,
                row: r,
                col: c,
                width: (cell.width as usize).max(1),
            });
        }
    }

    let bytes = line.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        let Some(scheme_len) = scheme_prefix_len_at(&line, idx) else {
            idx += line[idx..].chars().next().map_or(1, char::len_utf8);
            continue;
        };

        // Require start of line or non-alphanumeric boundary before scheme.
        if idx > 0 {
            if let Some(prev_ch) = line[..idx].chars().next_back() {
                if prev_ch.is_alphanumeric() {
                    idx += 1;
                    continue;
                }
            }
        }

        // Scan forward through valid URL characters.
        let mut end = idx + scheme_len;
        while end < bytes.len() {
            let ch = line[end..].chars().next().unwrap_or(' ');
            if is_url_body_char(ch) {
                end += ch.len_utf8();
            } else {
                break;
            }
        }

        let trimmed_end = trim_url_trailing(&line[idx..end]) + idx;
        let candidate = &line[idx..trimmed_end];
        if is_allowed_url_scheme(candidate) {
            if let Some((start_row, start_col, end_row, end_col)) =
                byte_range_to_cells(&entries, idx, trimmed_end, cols)
            {
                if cell_in_span(
                    target_row,
                    target_col,
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                ) {
                    return Some(HoveredLink {
                        start_row,
                        start_col,
                        end_row,
                        end_col,
                        url: candidate.to_owned(),
                        kind: LinkKind::PlainUrl,
                    });
                }
            }
        }

        idx = trimmed_end.max(idx + 1);
    }

    None
}

fn scheme_prefix_len_at(s: &str, idx: usize) -> Option<usize> {
    let tail = &s[idx..];
    if strip_ascii_prefix_ignore_case(tail, "https://").is_some() {
        Some("https://".len())
    } else if strip_ascii_prefix_ignore_case(tail, "http://").is_some() {
        Some("http://".len())
    } else if strip_ascii_prefix_ignore_case(tail, "mailto:").is_some() {
        Some("mailto:".len())
    } else {
        None
    }
}

const fn is_url_body_char(ch: char) -> bool {
    if ch.is_whitespace() || (ch as u32) <= 0x1F || (ch as u32) == 0x7F {
        return false;
    }
    !matches!(ch, '<' | '>' | '"' | '\'' | '`' | '{' | '}' | '|' | '\\' | '^')
}

/// Trim trailing sentence punctuation and unbalanced closing brackets (`)`, `]`).
fn trim_url_trailing(raw: &str) -> usize {
    let mut s = raw;
    while let Some(last) = s.chars().next_back() {
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => {
                let open = s.bytes().filter(|&b| b == b'(').count();
                let close = s.bytes().filter(|&b| b == b')').count();
                close > open
            }
            ']' => {
                let open = s.bytes().filter(|&b| b == b'[').count();
                let close = s.bytes().filter(|&b| b == b']').count();
                close > open
            }
            _ => false,
        };
        if strip {
            s = &s[..s.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    s.len()
}

fn byte_range_to_cells(
    entries: &[CellSpanEntry],
    byte_start: usize,
    byte_end: usize,
    cols: usize,
) -> Option<(usize, usize, usize, usize)> {
    let first = entries.iter().find(|e| e.byte_end > byte_start)?;
    let last = entries.iter().rev().find(|e| e.byte_start < byte_end)?;
    Some((
        first.row,
        first.col,
        last.row,
        (last.col + last.width).min(cols),
    ))
}

#[inline]
const fn cell_in_span(
    row: usize,
    col: usize,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
) -> bool {
    if row < start_row || row > end_row {
        return false;
    }
    if row == start_row && col < start_col {
        return false;
    }
    if row == end_row && col >= end_col {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use term_cells::Cell;

    use super::*;

    fn make_screen(cols: u16, rows: u16, lines: &[(&str, bool)]) -> Screen {
        let mut s = Screen::new(cols, rows);
        for (r, (text, wrapped)) in lines.iter().enumerate() {
            if r >= rows as usize {
                break;
            }
            s.wrapped[r] = *wrapped;
            let mut c = 0usize;
            for ch in text.chars() {
                if c >= cols as usize {
                    break;
                }
                let is_wide = (ch as u32) >= 0x3000;
                if is_wide && c + 1 < cols as usize {
                    s.cells[r * cols as usize + c] = Cell {
                        content: ch as u32,
                        width: 2,
                        ..Cell::BLANK
                    };
                    s.cells[r * cols as usize + c + 1] = Cell {
                        content: 0,
                        width: 0,
                        ..Cell::BLANK
                    };
                    c += 2;
                } else {
                    s.cells[r * cols as usize + c] = Cell {
                        content: ch as u32,
                        width: 1,
                        ..Cell::BLANK
                    };
                    c += 1;
                }
            }
        }
        s
    }

    struct MockLinkScreen {
        screen: Screen,
        links: HashMap<(usize, usize), String>,
    }

    impl ScreenView for MockLinkScreen {
        fn cells_screen(&self) -> &Screen {
            &self.screen
        }

        fn hyperlink_at(&self, row: usize, col: usize) -> Option<&str> {
            self.links.get(&(row, col)).map(String::as_str)
        }
    }

    #[test]
    fn allowed_scheme_filter_allows_http_https_mailto_and_rejects_others() {
        assert!(is_allowed_url_scheme("https://example.com"));
        assert!(is_allowed_url_scheme("http://localhost:8080/path?q=1#frag"));
        assert!(is_allowed_url_scheme("HTTPS://EXAMPLE.COM/A"));
        assert!(is_allowed_url_scheme("mailto:ivan@example.com"));
        assert!(is_allowed_url_scheme("MAILTO:user+tag@sub.example.org?subject=hi"));

        // Rejected schemes & malformed URLs.
        assert!(!is_allowed_url_scheme("file:///etc/passwd"));
        assert!(!is_allowed_url_scheme("FILE:///Users/lugo/secret"));
        assert!(!is_allowed_url_scheme("javascript:alert(1)"));
        assert!(!is_allowed_url_scheme("data:text/html,hi"));
        assert!(!is_allowed_url_scheme("ftp://example.com/file"));
        assert!(!is_allowed_url_scheme("ssh://host"));
        assert!(!is_allowed_url_scheme("https://"));
        assert!(!is_allowed_url_scheme("https:///no-host"));
        assert!(!is_allowed_url_scheme("mailto:"));
        assert!(!is_allowed_url_scheme("mailto:not-an-address"));
        assert!(!is_allowed_url_scheme("mailto:@example.com"));
        assert!(!is_allowed_url_scheme("mailto:a@"));
        assert!(!is_allowed_url_scheme("https://example.com/has space"));
        assert!(!is_allowed_url_scheme("https://example.com/ctrl\nline"));
    }

    #[test]
    fn plain_url_single_row_and_balanced_punctuation_trimming() {
        let s = make_screen(
            80,
            4,
            &[
                ("see (https://example.com/a/b).", false),
                ("wiki https://en.wikipedia.org/wiki/Rust_(lang) ok", false),
                ("contact mailto:dev@glyph3d.dev, thanks!", false),
                ("blocked file:///tmp/a and foohttps://bad.test", false),
            ],
        );

        // Row 0: `(https://example.com/a/b).` -> cols 5..28 (outer `)` and `.` stripped).
        let l0 = link_at_cell(&s, 0, 10).expect("https link");
        assert_eq!(l0.url, "https://example.com/a/b");
        assert_eq!(l0.kind, LinkKind::PlainUrl);
        assert_eq!((l0.start_row, l0.start_col, l0.end_row, l0.end_col), (0, 5, 0, 28));
        assert_eq!(link_at_cell(&s, 0, 4), None, "opening paren is not part of link");
        assert_eq!(link_at_cell(&s, 0, 28), None, "closing paren is trimmed");

        // Row 1: balanced parens preserved inside URL.
        let l1 = link_at_cell(&s, 1, 20).expect("wiki link");
        assert_eq!(l1.url, "https://en.wikipedia.org/wiki/Rust_(lang)");
        assert_eq!((l1.start_row, l1.start_col, l1.end_row, l1.end_col), (1, 5, 1, 46));

        // Row 2: mailto with trailing comma trimmed.
        let l2 = link_at_cell(&s, 2, 12).expect("mailto link");
        assert_eq!(l2.url, "mailto:dev@glyph3d.dev");
        assert_eq!((l2.start_row, l2.start_col, l2.end_row, l2.end_col), (2, 8, 2, 30));

        // Row 3: file:// rejected and mid-word `foohttps://` ignored.
        assert_eq!(link_at_cell(&s, 3, 12), None);
        assert_eq!(link_at_cell(&s, 3, 35), None);
    }

    #[test]
    fn plain_url_matches_across_soft_wrapped_rows() {
        // 20-col terminal where URL wraps from row 0 col 4 through row 1 col 15.
        let s = make_screen(
            20,
            3,
            &[
                ("go: https://glyph3d.", true),
                ("dev/docs/guide) done", false),
                ("plain text here", false),
            ],
        );

        let expected = HoveredLink {
            start_row: 0,
            start_col: 4,
            end_row: 1,
            end_col: 14,
            url: "https://glyph3d.dev/docs/guide".to_owned(),
            kind: LinkKind::PlainUrl,
        };
        assert_eq!(link_at_cell(&s, 0, 4), Some(expected.clone()));
        assert_eq!(link_at_cell(&s, 0, 19), Some(expected.clone()));
        assert_eq!(link_at_cell(&s, 1, 0), Some(expected.clone()));
        assert_eq!(link_at_cell(&s, 1, 13), Some(expected));
        assert_eq!(link_at_cell(&s, 1, 14), None, "trimmed ')'");
        assert_eq!(link_at_cell(&s, 0, 3), None);
    }

    #[test]
    fn osc8_span_takes_precedence_wraps_and_respects_scheme_filter() {
        // 12-col screen: OSC 8 link wraps from row 0 cols 6..12 to row 1 cols 0..4 (including wide '漢').
        let screen = make_screen(
            12,
            3,
            &[
                ("click here!!", true),
                ("漢ok tail", false),
                ("https://ok.x", false),
            ],
        );
        let mut links = HashMap::new();
        for c in 6..12 {
            links.insert((0, c), "https://glyph3d.dev/osc8".to_owned());
        }
        // Wide '漢' occupies cols 0 and 1, followed by 'o' (2) and 'k' (3).
        for c in 0..4 {
            links.insert((1, c), "https://glyph3d.dev/osc8".to_owned());
        }
        // Disallowed OSC 8 scheme sitting on top of plain `https://ok.x` must suppress fallback.
        for c in 0..12 {
            links.insert((2, c), "file:///etc/hosts".to_owned());
        }
        let view = MockLinkScreen { screen, links };

        let expected = HoveredLink {
            start_row: 0,
            start_col: 6,
            end_row: 1,
            end_col: 4,
            url: "https://glyph3d.dev/osc8".to_owned(),
            kind: LinkKind::Osc8,
        };
        assert_eq!(link_at_cell(&view, 0, 6), Some(expected.clone()));
        assert_eq!(link_at_cell(&view, 1, 0), Some(expected.clone()));
        assert_eq!(
            link_at_cell(&view, 1, 1),
            Some(expected),
            "trailing half of wide char inside OSC 8 link resolves the same span"
        );
        assert_eq!(link_at_cell(&view, 0, 5), None);
        assert_eq!(link_at_cell(&view, 1, 4), None);
        assert_eq!(
            link_at_cell(&view, 2, 2),
            None,
            "disallowed OSC 8 scheme must not open or fall back to plain URL"
        );
    }

    #[test]
    fn screen_impl_forwards_osc8_hyperlinks_and_soft_wrapped_ranges() {
        let mut screen = make_screen(
            12,
            3,
            &[
                ("click here!!", true),
                ("more text   ", false),
                ("https://a.co", false),
            ],
        );
        screen.hyperlinks.push("https://glyph3d.dev/direct".to_owned());
        screen.hyperlinks.push("file:///etc/passwd".to_owned());
        screen.link_spans = vec![
            term_cells::LinkSpan { row: 0, start_col: 6, end_col: 12, link_id: 0 },
            term_cells::LinkSpan { row: 1, start_col: 0, end_col: 4, link_id: 0 },
            term_cells::LinkSpan { row: 2, start_col: 0, end_col: 12, link_id: 1 },
        ];

        assert_eq!(ScreenView::hyperlink_at(&screen, 0, 5), None);
        assert_eq!(
            ScreenView::hyperlink_at(&screen, 0, 6),
            Some("https://glyph3d.dev/direct")
        );
        assert_eq!(
            ScreenView::hyperlink_at(&screen, 1, 2),
            Some("https://glyph3d.dev/direct")
        );

        let expected = HoveredLink {
            start_row: 0,
            start_col: 6,
            end_row: 1,
            end_col: 4,
            url: "https://glyph3d.dev/direct".to_owned(),
            kind: LinkKind::Osc8,
        };
        assert_eq!(link_at_cell(&screen, 0, 7), Some(expected.clone()));
        assert_eq!(link_at_cell(&screen, 1, 1), Some(expected));
        assert_eq!(link_at_cell(&screen, 1, 4), None);
        assert_eq!(
            link_at_cell(&screen, 2, 3),
            None,
            "disallowed file:// OSC 8 on Screen suppresses plain URL fallback"
        );
    }
}
