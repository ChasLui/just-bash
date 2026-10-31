//! Small text utilities that upstream ships as separate commands:
//! `rev`, `fold`, `expand`, `unexpand`, `strings`, `paste`, `column` and
//! `split`.
//!
//! They are pure functions over text (or over bytes for `strings`/`split`), so
//! they live here rather than in the executor; the builtin arms in `lib.rs`
//! only handle argument parsing and input resolution.

/// `rev`: reverse the characters of every line.
pub fn rev(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.extend(line.chars().rev());
        out.push('\n');
    }
    out
}

/// `fold -w N [-s]`: wrap lines at `width` display columns.
///
/// `-s` breaks at the last blank before the limit instead of mid-word, and a
/// single word longer than `width` is still hard-broken.
pub fn fold(text: &str, width: usize, break_words: bool) -> String {
    if width == 0 {
        return text.to_string();
    }
    let mut out = String::new();
    for line in text.lines() {
        let chars: Vec<char> = line.chars().collect();
        if chars.len() <= width {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let mut start = 0usize;
        while start < chars.len() {
            let mut end = (start + width).min(chars.len());
            let mut trimmed = false;
            if end < chars.len() && break_words {
                // Break after the last blank that fits, keeping it on the line
                // exactly as `fold -s` does.
                let window = &chars[start..end];
                if let Some(index) = window
                    .iter()
                    .rposition(|ch| *ch == ' ')
                    .filter(|index| index + 1 < window.len())
                {
                    end = start + index + 1;
                    trimmed = true;
                }
            }
            let piece: String = chars[start..end].iter().collect();
            out.push_str(&piece);
            out.push('\n');
            start = end;
            if trimmed {
                while start < chars.len() && chars[start] == ' ' {
                    start += 1;
                }
            }
        }
    }
    out
}

/// `expand [-t N]`: turn leading tabs into spaces up to the next tab stop.
pub fn expand(text: &str, tabstop: usize) -> String {
    let tabstop = tabstop.max(1);
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let mut column = 0usize;
        for ch in line.chars() {
            match ch {
                '\t' => {
                    let spaces = tabstop - (column % tabstop);
                    for _ in 0..spaces {
                        out.push(' ');
                    }
                    column += spaces;
                }
                other => {
                    out.push(other);
                    column += 1;
                }
            }
        }
    }
    // `lines()` drops a trailing newline; restore it when the input had one.
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `unexpand [-a|-t N]`: turn runs of spaces into tabs.
///
/// Without `-a` only leading blanks are converted, which is what the default
/// (`unexpand` with no flags) does.
pub fn unexpand(text: &str, tabstop: usize, all: bool) -> String {
    let tabstop = tabstop.max(1);
    let mut out = String::with_capacity(text.len());
    for (line_index, line) in text.lines().enumerate() {
        if line_index > 0 {
            out.push('\n');
        }
        let mut column = 0usize;
        let mut leading = true;
        // Blanks buffered until the next non-blank character is known.
        let mut pending = 0usize;
        let flush = |out: &mut String, column: &mut usize, pending: &mut usize| {
            while *pending > 0 {
                let advance = tabstop - (*column % tabstop);
                if advance <= *pending {
                    out.push('\t');
                    *column += advance;
                    *pending -= advance;
                } else {
                    for _ in 0..*pending {
                        out.push(' ');
                    }
                    *column += *pending;
                    *pending = 0;
                }
            }
        };
        for ch in line.chars() {
            match ch {
                ' ' if all || leading => pending += 1,
                '\t' if all || leading => {
                    // A tab is already a tab stop; flush any buffered blanks
                    // first, then keep the tab.
                    flush(&mut out, &mut column, &mut pending);
                    column += tabstop - (column % tabstop);
                    out.push('\t');
                }
                other => {
                    flush(&mut out, &mut column, &mut pending);
                    if other != ' ' {
                        leading = false;
                    }
                    out.push(other);
                    column += 1;
                }
            }
        }
        // Trailing blanks stay spaces.
        for _ in 0..pending {
            out.push(' ');
        }
    }
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `strings [-n N]`: print runs of printable characters at least `min` long.
pub fn strings(bytes: &[u8], min: usize) -> String {
    let min = min.max(1);
    let mut out = String::new();
    let mut current = String::new();
    let push_run = |run: &mut String, out: &mut String| {
        if run.chars().count() >= min {
            out.push_str(run);
            out.push('\n');
        }
        run.clear();
    };
    for byte in bytes {
        // Printable ASCII plus tab; anything else ends the run.
        if (0x20..0x7f).contains(byte) || *byte == b'\t' {
            current.push(*byte as char);
        } else {
            push_run(&mut current, &mut out);
        }
    }
    push_run(&mut current, &mut out);
    out
}

/// `paste [-d LIST] [-s] FILE…`
pub fn paste(inputs: &[Vec<String>], delimiters: &[char], serial: bool) -> String {
    let delimiter_at = |index: usize| -> char {
        if delimiters.is_empty() {
            '\t'
        } else {
            delimiters[index % delimiters.len()]
        }
    };
    let mut out = String::new();
    if serial {
        // `-s`: each file becomes one line, its lines joined by the delimiters.
        for lines in inputs {
            for (index, line) in lines.iter().enumerate() {
                out.push_str(line);
                if index + 1 < lines.len() {
                    out.push(delimiter_at(index));
                }
            }
            out.push('\n');
        }
        return out;
    }
    let rows = inputs.iter().map(Vec::len).max().unwrap_or(0);
    for row in 0..rows {
        for (column, lines) in inputs.iter().enumerate() {
            if let Some(line) = lines.get(row) {
                out.push_str(line);
            }
            if column + 1 < inputs.len() {
                out.push(delimiter_at(column));
            }
        }
        out.push('\n');
    }
    out
}

/// `column -t [-s SEP]`: align whitespace- (or `SEP`-) separated fields into
/// columns padded with two spaces.
pub fn column(text: &str, separator: Option<char>) -> String {
    let split = |line: &str| -> Vec<String> {
        match separator {
            Some(sep) => line.split(sep).map(str::to_string).collect(),
            None => line.split_whitespace().map(str::to_string).collect(),
        }
    };
    let rows: Vec<Vec<String>> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(split)
        .collect();
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in &rows {
        let mut line = String::new();
        for (index, cell) in row.iter().enumerate() {
            line.push_str(cell);
            // The final column is not padded, so no trailing spaces.
            if index + 1 < row.len() {
                let pad = widths[index] - cell.chars().count() + 2;
                for _ in 0..pad {
                    line.push(' ');
                }
            }
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// `split [-l N | -b N] [PREFIX]`: cut `text` into chunks.
///
/// Returns one `(name, contents)` pair per chunk; names are `PREFIX` + `aa`,
/// `ab`, … in the traditional two-letter form.
pub fn split(
    text: &str,
    prefix: &str,
    lines: Option<usize>,
    bytes: Option<usize>,
) -> Vec<(String, String)> {
    let mut chunks: Vec<String> = Vec::new();
    if let Some(size) = bytes {
        let size = size.max(1);
        let raw = text.as_bytes();
        for chunk in raw.chunks(size) {
            chunks.push(String::from_utf8_lossy(chunk).into_owned());
        }
    } else {
        let size = lines.unwrap_or(1000).max(1);
        let all: Vec<&str> = text.lines().collect();
        for chunk in all.chunks(size) {
            let mut piece = String::new();
            for line in chunk {
                piece.push_str(line);
                piece.push('\n');
            }
            chunks.push(piece);
        }
    }
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, contents)| {
            // xaa, xab … xaz, xba …
            let first = (b'a' as usize + (index / 26) % 26) as u8 as char;
            let second = (b'a' as usize + index % 26) as u8 as char;
            (format!("{prefix}{first}{second}"), contents)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rev_reverses_each_line() {
        assert_eq!(rev("hello\nworld\n"), "olleh\ndlrow\n");
    }

    #[test]
    fn fold_wraps_at_width() {
        assert_eq!(
            fold("hello world test\n", 5, false),
            "hello\n worl\nd tes\nt\n"
        );
    }

    #[test]
    fn fold_breaks_at_blanks_when_asked() {
        assert_eq!(
            fold("hello world foo bar\n", 10, true),
            "hello \nworld foo \nbar\n"
        );
    }

    #[test]
    fn expand_uses_tab_stops() {
        assert_eq!(expand("\thello\n", 8), "        hello\n");
        assert_eq!(expand("\thello\n", 4), "    hello\n");
    }

    #[test]
    fn unexpand_rebuilds_tabs() {
        assert_eq!(unexpand("        hello\n", 8, false), "\thello\n");
        assert_eq!(unexpand("hello   world\n", 8, true), "hello\tworld\n");
    }

    #[test]
    fn strings_keeps_runs_of_printable_bytes() {
        assert_eq!(strings(b"ab\x00cd\x00efgh", 4), "efgh\n");
        assert_eq!(strings(b"ab\x00cde\x00fghi", 3), "cde\nfghi\n");
    }

    #[test]
    fn paste_joins_files_side_by_side() {
        let a = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let b = vec!["1".to_string(), "2".to_string()];
        assert_eq!(
            paste(&[a.clone(), b.clone()], &['\t'], false),
            "a\t1\nb\t2\nc\t\n"
        );
        assert_eq!(paste(&[a.clone(), b], &[','], false), "a,1\nb,2\nc,\n");
        assert_eq!(paste(&[a], &[','], true), "a,b,c\n");
    }

    #[test]
    fn column_aligns_fields() {
        assert_eq!(column("a b c\nd e\nf\n", None), "a  b  c\nd  e\nf\n");
        assert_eq!(
            // Widest cell in column 0 is `longer` (6), so `short` is padded
            // with 3 spaces to reach the two-space gap bash emits.
            column("short long\nlonger x\n", None),
            "short   long\nlonger  x\n"
        );
    }

    #[test]
    fn split_by_lines_and_bytes() {
        let by_lines = split("a\nb\nc\n", "x", Some(1), None);
        assert_eq!(by_lines.len(), 3);
        assert_eq!(by_lines[0].0, "xaa");
        assert_eq!(by_lines[0].1, "a\n");
        assert_eq!(by_lines[1].0, "xab");

        let by_bytes = split("hello", "x", None, Some(4));
        assert_eq!(by_bytes[0].1, "hell");
        assert_eq!(by_bytes[1].1, "o");
    }
}

/// Numbering style for `nl -n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NlNumberFormat {
    /// Right-aligned, blank-padded (`-n rn`, the default).
    Right,
    /// Left-aligned (`-n ln`).
    Left,
    /// Zero-padded (`-n rz`).
    RightZero,
}

/// Options for `nl`.
#[derive(Debug, Clone)]
pub struct NlOptions {
    pub width: usize,
    pub separator: String,
    pub start: i64,
    pub increment: i64,
    pub format: NlNumberFormat,
    /// `-ba` numbers every line; the default numbers only non-blank lines.
    pub number_blank: bool,
}

impl Default for NlOptions {
    fn default() -> Self {
        Self {
            width: 6,
            separator: "\t".to_string(),
            start: 1,
            increment: 1,
            format: NlNumberFormat::Right,
            number_blank: false,
        }
    }
}

/// `nl`: number the lines of `text`.
pub fn nl(text: &str, options: &NlOptions) -> String {
    let mut out = String::new();
    let mut number = options.start;
    for line in text.lines() {
        let blank = line.trim().is_empty();
        if blank && !options.number_blank {
            // An unnumbered blank line still occupies the number column and the
            // separator, blanked out — matching `nl`'s output for empty lines.
            for _ in 0..options.width {
                out.push(' ');
            }
            out.push_str(&options.separator);
            out.push('\n');
            continue;
        }
        let label = match options.format {
            NlNumberFormat::Right => format!("{:>width$}", number, width = options.width),
            NlNumberFormat::Left => format!("{:<width$}", number, width = options.width),
            NlNumberFormat::RightZero => format!("{:0>width$}", number, width = options.width),
        };
        out.push_str(&label);
        out.push_str(&options.separator);
        out.push_str(line);
        out.push('\n');
        number += options.increment;
    }
    out
}

/// Options for `join`.
#[derive(Debug, Clone)]
pub struct JoinOptions {
    /// Field separator; `None` means runs of blanks.
    pub separator: Option<char>,
    /// Join field for the first file (1-based).
    pub first_key: usize,
    /// Join field for the second file (1-based).
    pub second_key: usize,
    /// Print unpairable lines from the first file (`-a 1`).
    pub include_first: bool,
    /// Print unpairable lines from the second file (`-a 2`).
    pub include_second: bool,
    /// Print only unpairable lines (`-v 1` / `-v 2`).
    pub only_unpaired: Option<u8>,
}

impl Default for JoinOptions {
    fn default() -> Self {
        Self {
            separator: None,
            first_key: 1,
            second_key: 1,
            include_first: false,
            include_second: false,
            only_unpaired: None,
        }
    }
}

fn join_fields(line: &str, separator: Option<char>) -> Vec<&str> {
    match separator {
        Some(sep) => line.split(sep).collect(),
        None => line.split_whitespace().collect(),
    }
}

/// `join`: merge two sorted files on a key field.
pub fn join(first: &str, second: &str, options: &JoinOptions) -> String {
    let first_lines: Vec<&str> = first.lines().filter(|line| !line.is_empty()).collect();
    let second_lines: Vec<&str> = second.lines().filter(|line| !line.is_empty()).collect();
    let separator = options.separator;
    let joiner = separator.unwrap_or(' ').to_string();

    let key_of = |line: &str, index: usize| -> String {
        join_fields(line, separator)
            .get(index - 1)
            .copied()
            .unwrap_or("")
            .to_string()
    };
    // Everything except the key field, in file order.
    let rest_of = |line: &str, index: usize| -> Vec<String> {
        join_fields(line, separator)
            .into_iter()
            .enumerate()
            .filter(|(position, _)| *position + 1 != index)
            .map(|(_, value)| value.to_string())
            .collect()
    };

    let mut used_second = vec![false; second_lines.len()];
    let mut out = String::new();

    for first_line in &first_lines {
        let key = key_of(first_line, options.first_key);
        let mut matched = false;
        for (index, second_line) in second_lines.iter().enumerate() {
            if key_of(second_line, options.second_key) != key {
                continue;
            }
            matched = true;
            used_second[index] = true;
            if matches!(options.only_unpaired, Some(1)) {
                continue;
            }
            let mut row = vec![key.clone()];
            row.extend(rest_of(first_line, options.first_key));
            row.extend(rest_of(second_line, options.second_key));
            out.push_str(&row.join(&joiner));
            out.push('\n');
        }
        if !matched && (options.include_first || options.only_unpaired == Some(1)) {
            let mut row = vec![key.clone()];
            row.extend(rest_of(first_line, options.first_key));
            out.push_str(&row.join(&joiner));
            out.push('\n');
        }
    }

    if options.include_second || options.only_unpaired == Some(2) {
        for (index, second_line) in second_lines.iter().enumerate() {
            if used_second[index] {
                continue;
            }
            let mut row = vec![key_of(second_line, options.second_key)];
            row.extend(rest_of(second_line, options.second_key));
            out.push_str(&row.join(&joiner));
            out.push('\n');
        }
    }

    out
}

/// One `find` predicate. Only the forms the upstream fixtures exercise are
/// modelled; anything else is rejected by the caller as unsupported.
#[derive(Debug, Clone)]
pub enum FindTest {
    /// `-name GLOB`
    Name(String),
    /// `-type f|d`
    Type(char),
}

/// A `find` expression: a conjunction branch, optionally OR-ed with another.
#[derive(Debug, Clone, Default)]
pub struct FindExpression {
    /// Paths must satisfy every test in `and` (or any of the `or` branches).
    pub and: Vec<FindTest>,
    pub or: Option<Vec<FindTest>>,
}

impl FindExpression {
    fn matches(&self, name: &str, kind: char) -> bool {
        let matches_branch = |branch: &[FindTest]| -> bool {
            branch.iter().all(|test| match test {
                FindTest::Name(pattern) => just_bash_glob::pattern_matches(pattern, name),
                FindTest::Type(wanted) => *wanted == kind,
            })
        };
        matches_branch(&self.and)
            || self
                .or
                .as_ref()
                .is_some_and(|branch| matches_branch(branch))
    }
}

/// Options for `find`.
#[derive(Debug, Clone)]
pub struct FindOptions {
    pub max_depth: Option<usize>,
    pub min_depth: usize,
    pub expression: FindExpression,
}

/// A visited path: display path, whether it is a directory, and its depth.
pub type FindEntry<'a> = (&'a str, bool, usize);

/// `find`: filter the visited entries and render them in visit order.
pub fn find(entries: &[FindEntry<'_>], options: &FindOptions) -> String {
    let mut out = String::new();
    for (display, is_dir, depth) in entries {
        let depth = *depth;
        if depth < options.min_depth {
            continue;
        }
        if options.max_depth.is_some_and(|max| depth > max) {
            continue;
        }
        // `-name` matches the basename, not the whole path.
        let name = display.rsplit('/').next().unwrap_or(display);
        let kind = if *is_dir { 'd' } else { 'f' };
        if !options.expression.matches(name, kind) {
            continue;
        }
        out.push_str(display);
        out.push('\n');
    }
    out
}

/// Base64-encode raw bytes (no trailing newline).
pub fn base64_encode(bytes: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (first << 16) | (second << 8) | third;
        out.push(ALPHABET[(combined >> 18) as usize & 0x3f]);
        out.push(ALPHABET[(combined >> 12) as usize & 0x3f]);
        out.push(if chunk.len() > 1 {
            ALPHABET[(combined >> 6) as usize & 0x3f]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[combined as usize & 0x3f]
        } else {
            b'='
        });
    }
    out
}

/// Decode base64 text into raw bytes; non-alphabet characters (whitespace,
/// padding) are ignored, so the result is byte-exact.
pub fn base64_decode(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    out
}

/// `uname [-s] [-n] [-r] [-m] [-a]`: report the virtual platform identity.
pub fn uname(flags: &str) -> String {
    const SYSNAME: &str = "Linux";
    const NODENAME: &str = "just-bash";
    const RELEASE: &str = "6.0.0-just-bash";
    const MACHINE: &str = "x86_64";
    let mut parts: Vec<&str> = Vec::new();
    if flags.contains('a') {
        parts.extend([SYSNAME, NODENAME, RELEASE, MACHINE]);
    } else {
        // `uname` with no flags prints the system name.
        let mut any = false;
        if flags.contains('s') || flags.is_empty() {
            parts.push(SYSNAME);
            any = true;
        }
        if flags.contains('n') {
            parts.push(NODENAME);
            any = true;
        }
        if flags.contains('r') {
            parts.push(RELEASE);
            any = true;
        }
        if flags.contains('m') {
            parts.push(MACHINE);
            any = true;
        }
        if !any {
            parts.push(SYSNAME);
        }
    }
    parts.join(" ")
}

/// `xxd [-p] [-r]`: hex dump, or the reverse.
///
/// The default layout is `OFFSET: hhhh hhhh …  ascii`, 16 bytes per line, which
/// is what `xxd` without flags produces. `-p` switches to a bare hex stream.
pub fn xxd(bytes: &[u8], plain: bool, reverse: bool) -> String {
    if reverse {
        return xxd_reverse(bytes, plain);
    }
    if plain {
        let mut out = String::with_capacity(bytes.len() * 2 + 1);
        for byte in bytes {
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0xf));
        }
        out.push('\n');
        return out;
    }

    let mut out = String::new();
    for (index, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}: ", index * 16));
        // Two groups of two bytes, then the padding up to a full line.
        for group in 0..8 {
            let position = group * 2;
            match (chunk.get(position), chunk.get(position + 1)) {
                (Some(first), Some(second)) => {
                    out.push(hex_digit(first >> 4));
                    out.push(hex_digit(first & 0xf));
                    out.push(hex_digit(second >> 4));
                    out.push(hex_digit(second & 0xf));
                }
                (Some(first), None) => {
                    out.push(hex_digit(first >> 4));
                    out.push(hex_digit(first & 0xf));
                    out.push_str("  ");
                }
                _ => out.push_str("    "),
            }
            out.push(' ');
        }
        // One more space separates the hex groups from the ASCII column.
        out.push(' ');
        // The ASCII column: printable bytes as-is, everything else as `.`.
        for byte in chunk {
            out.push(if (0x20..0x7f).contains(byte) {
                *byte as char
            } else {
                '.'
            });
        }
        out.push('\n');
    }
    out
}

fn hex_digit(value: u8) -> char {
    char::from_digit(value as u32, 16).unwrap_or('0')
}

/// Rebuild bytes from `xxd` output, honouring `-p` (bare hex) and the default
/// `OFFSET: hhhh hhhh  ascii` layout.
fn xxd_reverse(bytes: &[u8], plain: bool) -> String {
    decode_latin1(&xxd_reverse_bytes(bytes, plain))
}

/// Byte-exact `xxd`: reverse mode returns the reconstructed raw bytes.
pub fn xxd_bytes(bytes: &[u8], plain: bool, reverse: bool) -> Vec<u8> {
    if reverse {
        xxd_reverse_bytes(bytes, plain)
    } else {
        xxd(bytes, plain, false).into_bytes()
    }
}

/// Rebuild the raw bytes from `xxd` output without any lossy text conversion.
fn xxd_reverse_bytes(bytes: &[u8], plain: bool) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for line in text.lines() {
        let hex = if plain {
            line.trim().to_string()
        } else {
            // Keep only the hex half of the line: everything after the colon up
            // to the two-space gap that introduces the ASCII column.
            let body = match line.split_once(':') {
                Some((_, rest)) => rest,
                None => continue,
            };
            let hex_end = body.find("  ").unwrap_or(body.len());
            body[..hex_end].replace(' ', "")
        };
        let mut digits = hex.chars().filter(|ch| ch.is_ascii_hexdigit());
        while let (Some(high), Some(low)) = (digits.next(), digits.next()) {
            let pair: String = [high, low].iter().collect();
            match u8::from_str_radix(&pair, 16) {
                Ok(byte) => out.push(byte),
                Err(_) => break,
            }
        }
    }
    // The reconstructed bytes are returned as-is; the string wrapper above maps
    // them one-code-point-per-byte for callers that want text.
    out
}

/// Bytes to text, one code point per byte (the inverse of [`encode_latin1`]).
pub fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| *byte as char).collect()
}

#[cfg(test)]
mod xxd_tests {
    use super::*;

    #[test]
    fn default_layout_matches_xxd() {
        assert_eq!(
            xxd(b"hello\n", false, false),
            "00000000: 6865 6c6c 6f0a                           hello.\n"
        );
        assert_eq!(
            xxd(b"abc", false, false),
            "00000000: 6162 63                                  abc\n"
        );
    }

    #[test]
    fn long_input_wraps_at_sixteen_bytes() {
        let output = xxd(b"0123456789abcdef0123", false, false);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("00000000: "));
        assert!(lines[1].starts_with("00000010: "));
        assert!(lines[0].ends_with("0123456789abcdef"));
        assert!(lines[1].ends_with("0123"));
    }

    #[test]
    fn empty_input_produces_nothing() {
        assert_eq!(xxd(b"", false, false), "");
    }

    #[test]
    fn plain_mode_is_a_bare_hex_stream() {
        assert_eq!(xxd(b"hello\n", true, false), "68656c6c6f0a\n");
    }

    #[test]
    fn reverse_round_trips_both_layouts() {
        let dumped = xxd(b"hello\n", false, false);
        assert_eq!(xxd(dumped.as_bytes(), false, true), "hello\n");
        let plain = xxd(b"abc", true, false);
        assert_eq!(xxd(plain.as_bytes(), true, true), "abc");
    }

    /// `-r` must not corrupt bytes: `from_utf8_lossy` would turn `0xff` into
    /// the replacement character, losing the original value.
    #[test]
    fn reverse_keeps_bytes_outside_ascii() {
        let decoded = xxd(b"ff", true, true);
        assert_eq!(decoded.chars().count(), 1);
        assert_eq!(decoded.chars().next(), Some('\u{ff}'));
        // Re-encoding that character through the dump shows the same value.
        assert_eq!(xxd(decoded.as_bytes(), true, false).trim_end(), "c3bf");
    }

    /// Printable data round-trips exactly, which is the common case.
    #[test]
    fn reverse_round_trips_printable_bytes() {
        let decoded = xxd(b"414243", true, true);
        assert_eq!(decoded, "ABC");
        assert_eq!(xxd(decoded.as_bytes(), true, false), "414243\n");
    }

    #[test]
    fn non_printable_bytes_show_as_dots() {
        assert!(xxd(b"\x00\x01", false, false).ends_with("..\n"));
    }
}

/// Accumulated disk usage for one path, in 512-byte blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuUsage {
    pub path: String,
    /// Blocks used by this path alone (files only; directories report 0).
    pub own: u64,
    /// Blocks used by this path and everything below it.
    pub total: u64,
    pub is_dir: bool,
}

/// Format one `du` line: `BLOCKS<TAB>PATH`.
///
/// Sizes are in 512-byte blocks rounded up, which is what `du` reports by
/// default; `human` renders a `K`/`M` suffix instead.
pub fn du_line(usage: &DuUsage, human: bool) -> String {
    if human {
        format!("{}\t{}\n", human_blocks(usage.total), usage.path)
    } else {
        format!("{}\t{}\n", usage.total, usage.path)
    }
}

fn human_blocks(blocks: u64) -> String {
    let bytes = blocks * 512;
    if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// `tac [-s SEP]`: reverse the order of records.
///
/// With the default separator (newline) each line is emitted in reverse order,
/// keeping the original presence or absence of a trailing newline. A custom
/// separator reverses the text between separators.
pub fn tac(text: &str, separator: Option<&str>) -> String {
    match separator {
        None => {
            // `lines()` drops a trailing newline; remember whether one existed
            // so the output keeps the same shape.
            let had_trailing_newline = text.ends_with('\n');
            let mut lines: Vec<&str> = text.lines().collect();
            if text.is_empty() {
                return String::new();
            }
            lines.reverse();
            let mut out = lines.join("\n");
            if had_trailing_newline {
                out.push('\n');
            }
            out
        }
        Some(separator) => {
            // An empty separator is accepted; it yields no split points, so the
            // record list is the text itself.
            if separator.is_empty() || !text.contains(separator) {
                return text.to_string();
            }
            let mut pieces: Vec<&str> = text.split(separator).collect();
            pieces.reverse();
            pieces.join(separator)
        }
    }
}
