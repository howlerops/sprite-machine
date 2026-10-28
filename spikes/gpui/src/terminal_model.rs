//! Pure terminal model for the terminal pane: no GPUI here.
//!
//! - [`TerminalModel`] feeds raw output bytes through `alacritty_terminal`'s VT parser
//!   (`vte`) into a `Term`, the same pairing Zed's `crates/terminal` uses. Bare LFs get the
//!   pty's CR, so a line feed starts the next line at column zero.
//! - [`grid_rows`] maps the visible grid to [`StyledRow`]s: each row is a list of
//!   [`StyledRun`]s, where a run is a maximal stretch of cells with the same foreground colour
//!   and weight. The pane renders those runs; it never looks at cells itself.
//!
//! Colours: explicit SGR colours (`31`, `38;5;n`, `38;2;r;g;b`) resolve to a fixed xterm-style
//! palette; the default foreground is `None` so the view can use the theme's foreground colour.

use alacritty_terminal::event::{EventListener, VoidListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

/// Visible rows of the pane's terminal.
pub const SCREEN_LINES: usize = 30;
/// Visible columns of the pane's terminal.
pub const COLUMNS: usize = 100;
/// Rows kept above the screen.
pub const SCROLLBACK_LINES: usize = 1_000;

/// An opaque sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb8 {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// `0xRRGGBB`.
    pub fn to_u32(self) -> u32 {
        (u32::from(self.r) << 16) | (u32::from(self.g) << 8) | u32::from(self.b)
    }
}

/// The standard 16 ANSI colours (xterm defaults).
const ANSI_16: [Rgb8; 16] = [
    Rgb8::new(0x00, 0x00, 0x00),
    Rgb8::new(0xcd, 0x00, 0x00),
    Rgb8::new(0x00, 0xcd, 0x00),
    Rgb8::new(0xcd, 0xcd, 0x00),
    Rgb8::new(0x00, 0x00, 0xee),
    Rgb8::new(0xcd, 0x00, 0xcd),
    Rgb8::new(0x00, 0xcd, 0xcd),
    Rgb8::new(0xe5, 0xe5, 0xe5),
    Rgb8::new(0x7f, 0x7f, 0x7f),
    Rgb8::new(0xff, 0x00, 0x00),
    Rgb8::new(0x00, 0xff, 0x00),
    Rgb8::new(0xff, 0xff, 0x00),
    Rgb8::new(0x5c, 0x5c, 0xff),
    Rgb8::new(0xff, 0x00, 0xff),
    Rgb8::new(0x00, 0xff, 0xff),
    Rgb8::new(0xff, 0xff, 0xff),
];

/// SGR 31 (the palette's red).
pub const ANSI_RED: Rgb8 = ANSI_16[1];
/// SGR 32 (the palette's green).
pub const ANSI_GREEN: Rgb8 = ANSI_16[2];

/// Maximal run of cells with one foreground colour and weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledRun {
    pub text: String,
    /// Grid columns the run covers (a wide character covers two).
    pub columns: usize,
    /// `None` is the default foreground (the view substitutes the theme colour).
    pub fg: Option<Rgb8>,
    pub bold: bool,
}

/// One visible grid row, with trailing blank cells dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StyledRow {
    pub runs: Vec<StyledRun>,
}

impl StyledRow {
    /// The row's characters without styling.
    pub fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }
}

/// A `Term`'s dimensions, in the form `alacritty_terminal` takes them.
struct Size {
    lines: usize,
    columns: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// A VT terminal: bytes in, a grid and scrollback out.
pub struct TerminalModel {
    term: Term<VoidListener>,
    parser: Processor,
    /// Whether the last byte fed was a CR (so a following LF needs no CR inserted).
    prev_cr: bool,
}

impl TerminalModel {
    pub fn new(columns: usize, lines: usize) -> Self {
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let size = Size { lines, columns };
        Self {
            term: Term::new(config, &size, VoidListener),
            parser: Processor::new(),
            prev_cr: false,
        }
    }

    /// Advances the parser over `bytes`. Sequences may be split across calls.
    ///
    /// A bare LF becomes CR LF, as the pty's `onlcr` line discipline does for a real child
    /// process: the model is fed the child's bytes directly, so it does that step itself.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut start = 0;
        let mut prev_cr = self.prev_cr;
        for (index, &byte) in bytes.iter().enumerate() {
            if byte == b'\n' && !prev_cr {
                self.parser.advance(&mut self.term, &bytes[start..index]);
                self.parser.advance(&mut self.term, b"\r");
                start = index;
            }
            prev_cr = byte == b'\r';
        }
        self.parser.advance(&mut self.term, &bytes[start..]);
        self.prev_cr = prev_cr;
    }

    /// The visible grid, top row first.
    pub fn rows(&self) -> Vec<StyledRow> {
        grid_rows(&self.term)
    }

    /// Rows above the screen, not counting the screen itself.
    pub fn scrollback_len(&self) -> usize {
        self.term.grid().history_size()
    }

    pub fn term(&self) -> &Term<VoidListener> {
        &self.term
    }
}

/// One grid cell after the cell-level decisions: its character, colour, weight and width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    c: char,
    fg: Option<Rgb8>,
    bold: bool,
    width: usize,
}

impl Cell {
    /// A cell that prints nothing and keeps no style: trailing ones are trimmed.
    fn is_plain_blank(self) -> bool {
        self.c == ' ' && self.fg.is_none() && !self.bold
    }
}

/// The visible rows of `term` (the grid at display offset zero), top row first.
///
/// Spacer cells after a wide character are skipped (the character already covers both columns).
/// Trailing plain blanks are dropped, so an empty row has no runs.
pub fn grid_rows<T: EventListener>(term: &Term<T>) -> Vec<StyledRow> {
    let grid = term.grid();
    (0..grid.screen_lines())
        .map(|line| {
            let mut cells: Vec<Cell> = (0..grid.columns())
                .filter_map(|col| {
                    let cell = &grid[Line(line as i32)][Column(col)];
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                        || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        return None;
                    }
                    Some(Cell {
                        c: cell.c,
                        fg: resolve_color(cell.fg),
                        bold: cell.flags.contains(Flags::BOLD),
                        width: if cell.flags.contains(Flags::WIDE_CHAR) {
                            2
                        } else {
                            1
                        },
                    })
                })
                .collect();
            while cells.last().is_some_and(|cell| cell.is_plain_blank()) {
                cells.pop();
            }
            StyledRow {
                runs: group_runs(&cells),
            }
        })
        .collect()
}

/// Folds consecutive cells with the same colour and weight into runs.
fn group_runs(cells: &[Cell]) -> Vec<StyledRun> {
    let mut runs: Vec<StyledRun> = Vec::new();
    for cell in cells {
        match runs.last_mut() {
            Some(run) if run.fg == cell.fg && run.bold == cell.bold => {
                run.text.push(cell.c);
                run.columns += cell.width;
            }
            _ => runs.push(StyledRun {
                text: cell.c.to_string(),
                columns: cell.width,
                fg: cell.fg,
                bold: cell.bold,
            }),
        }
    }
    runs
}

/// The concrete colour of a cell's foreground, or `None` for the default foreground.
fn resolve_color(color: Color) -> Option<Rgb8> {
    match color {
        Color::Spec(Rgb { r, g, b }) => Some(Rgb8::new(r, g, b)),
        Color::Indexed(index) => Some(indexed_color(index)),
        Color::Named(named) => named_index(named).map(|index| ANSI_16[index]),
    }
}

/// The palette index of a named colour; `None` for the default and cursor colours.
/// Dim variants use the normal colour (the model does not dim).
fn named_index(named: NamedColor) -> Option<usize> {
    use NamedColor::*;
    Some(match named {
        Black | DimBlack => 0,
        Red | DimRed => 1,
        Green | DimGreen => 2,
        Yellow | DimYellow => 3,
        Blue | DimBlue => 4,
        Magenta | DimMagenta => 5,
        Cyan | DimCyan => 6,
        White | DimWhite => 7,
        BrightBlack => 8,
        BrightRed => 9,
        BrightGreen => 10,
        BrightYellow => 11,
        BrightBlue => 12,
        BrightMagenta => 13,
        BrightCyan => 14,
        BrightWhite => 15,
        Foreground | Background | Cursor | BrightForeground | DimForeground => return None,
    })
}

/// The 256-colour palette: 16 ANSI, a 6x6x6 colour cube, then 24 greys.
fn indexed_color(index: u8) -> Rgb8 {
    match index {
        0..=15 => ANSI_16[usize::from(index)],
        16..=231 => {
            let n = index - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            Rgb8::new(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232);
            Rgb8::new(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ANSI_GREEN, ANSI_RED, COLUMNS, SCREEN_LINES, StyledRun, TerminalModel};

    fn model_after(bytes: &[u8]) -> TerminalModel {
        let mut model = TerminalModel::new(COLUMNS, SCREEN_LINES);
        model.feed(bytes);
        model
    }

    fn run(text: &str, fg: Option<super::Rgb8>, bold: bool) -> StyledRun {
        StyledRun {
            text: text.to_owned(),
            columns: text.chars().count(),
            fg,
            bold,
        }
    }

    #[test]
    fn terminal_model_sgr_colours_resolve_to_palette_runs() {
        let model = model_after(b"\x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m plain\r\n");
        let rows = model.rows();
        assert_eq!(
            rows[0].runs,
            vec![
                run("red", Some(ANSI_RED), false),
                run(" ", None, false),
                run("green", Some(ANSI_GREEN), false),
                run(" plain", None, false),
            ]
        );
        assert_eq!(rows[0].text(), "red green plain");
    }

    #[test]
    fn terminal_model_bold_is_its_own_run() {
        let model = model_after(b"\x1b[1mbold\x1b[22m normal\r\n");
        assert_eq!(
            model.rows()[0].runs,
            vec![run("bold", None, true), run(" normal", None, false)]
        );
    }

    #[test]
    fn terminal_model_carriage_return_overwrites_the_line() {
        let model = model_after(b"progress 1/3\rprogress 2/3\r\n");
        assert_eq!(model.rows()[0].text(), "progress 2/3");
    }

    #[test]
    fn terminal_model_erase_line_clears_from_the_cursor() {
        // Move to column 3 (CUF 3), then erase from there to the end of the line (EL 0).
        let model = model_after(b"abcdef\r\x1b[3C\x1b[K\r\n");
        assert_eq!(model.rows()[0].text(), "abc");
    }

    #[test]
    fn terminal_model_wraps_a_long_line_onto_the_next_row() {
        let mut bytes = vec![b'x'; COLUMNS];
        bytes.extend_from_slice(b"yyyyy\r\n");
        let model = model_after(&bytes);
        let rows = model.rows();
        assert_eq!(rows[0].text(), "x".repeat(COLUMNS));
        assert_eq!(rows[1].text(), "yyyyy");
        assert!(rows[2].runs.is_empty());
    }

    #[test]
    fn terminal_model_lines_above_the_screen_go_to_scrollback() {
        let output: String = (0..40).map(|i| format!("line {i}\r\n")).collect();
        let model = model_after(output.as_bytes());
        // 40 lines plus the cursor's blank line: 41 rows, 30 on screen, 11 scrolled off.
        assert_eq!(model.scrollback_len(), 11);
        assert_eq!(model.rows()[0].text(), "line 11");
        assert_eq!(model.rows()[SCREEN_LINES - 1].text(), "");
    }

    #[test]
    fn terminal_model_line_feed_returns_to_column_zero() {
        let model = model_after(b"ab\ncd");
        let rows = model.rows();
        assert_eq!(rows[0].text(), "ab");
        assert_eq!(rows[1].text(), "cd");
    }

    #[test]
    fn terminal_model_lf_split_from_its_cr_across_chunks_is_not_doubled() {
        // "\r" then "\n" in the next chunk is already CRLF: no extra CR is inserted.
        let mut model = model_after(b"ab\r");
        model.feed(b"\ncd\n");
        let rows = model.rows();
        assert_eq!(rows[0].text(), "ab");
        assert_eq!(rows[1].text(), "cd");
        assert_eq!(rows[2].text(), "");
    }

    #[test]
    fn terminal_model_resolves_256_colour_cube_and_greys() {
        // 38;5;196 is pure red in the 6x6x6 cube; 38;5;244 is a mid grey.
        let model = model_after(b"\x1b[38;5;196mA\x1b[38;5;244mB\x1b[0m\r\n");
        let runs = &model.rows()[0].runs;
        assert_eq!(runs[0].fg, Some(super::Rgb8::new(255, 0, 0)));
        assert_eq!(runs[1].fg, Some(super::Rgb8::new(128, 128, 128)));
    }
}
