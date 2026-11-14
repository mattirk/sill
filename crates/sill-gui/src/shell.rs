//! Where one command of a shell line ends and the next begins, for showing a call that ran
//! several commands as several lines (docs/sill-gui.md, *Tool runs*). The rules are the POSIX
//! shell's, the same in sh, bash, dash and zsh: `;`, `&&`, `||` and a newline end a command
//! where they stand outside quotes, substitutions and subshells; a pipeline is one command.
//!
//! This reads only what it is sure of. A line with a heredoc, a loop, `if`, `case`, a brace
//! group or a function, or one whose quotes or brackets do not close, is not divided at all:
//! [`steps`] says `None` and the caller shows the line whole. What it does divide is never
//! rewritten: every step is a slice of the line, in order, and the steps with their operators
//! are the line, less the blanks at the cuts.

/// One command of a line, and the operator that ends it as it was written, with the blank
/// before it when it had one (`;`, ` &&`, ` ||`); empty after the last command and where a
/// newline ended it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step<'a> {
    pub text: &'a str,
    pub operator: &'a str,
}

/// Words that open or continue a compound command: a line with one where a command starts is
/// not divided.
const RESERVED: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "select", "function", "coproc", "[[",
];

/// The commands of `line` in the order the shell runs them, or `None` when the line holds
/// something this does not read.
pub fn steps(line: &str) -> Option<Vec<Step<'_>>> {
    let bytes = line.as_bytes();
    // (text start, text end, operator end): every significant byte is ASCII, so these are
    // character boundaries
    let mut pieces: Vec<(usize, usize, usize)> = Vec::new();
    let (mut start, mut at, mut command_next, mut word_start) = (0, 0, true, true);
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b' ' || byte == b'\t' || byte == b'\r' {
            word_start = true;
            at += 1;
            continue;
        }
        if word_start {
            let word = bare_word(bytes, at);
            if word == b"{" || word == b"}" {
                return None;
            }
            if command_next && !word.is_empty() {
                if RESERVED.iter().any(|reserved| reserved.as_bytes() == word) {
                    return None;
                }
                // `! command` and `time command`: the command is still to come
                command_next = word == b"!" || word == b"time";
            } else if command_next && byte != b'\n' && byte != b';' && byte != b'&' && byte != b'|'
            {
                command_next = false;
            }
        }
        let was_word_start = std::mem::replace(&mut word_start, false);
        match byte {
            b'\\' => at += 2,
            b'\'' => at = single_quoted(bytes, at)?,
            b'"' => at = double_quoted(bytes, at)?,
            b'`' => at = backquoted(bytes, at)?,
            b'$' => at = dollar(bytes, at, false)?,
            b'(' => at = parenthesized(bytes, at)?,
            b')' => return None,
            b'#' if was_word_start => {
                // a comment, to the end of its line
                at = bytes[at..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(bytes.len(), |end| at + end);
            }
            b'<' if bytes[at..].starts_with(b"<<<") => at += 3,
            b'<' if bytes[at..].starts_with(b"<<") => return None,
            b';' | b'\n' | b'&' | b'|' => {
                let rest = &bytes[at..];
                let operator = if rest.starts_with(b";;") || rest.starts_with(b";&") {
                    return None;
                } else if rest.starts_with(b"&&") || rest.starts_with(b"||") {
                    2
                } else if byte == b';' || byte == b'\n' {
                    1
                } else {
                    0
                };
                word_start = true;
                if operator > 0 {
                    pieces.push((start, at, at + operator));
                    at += operator;
                    start = at;
                    command_next = true;
                } else if byte == b'|' {
                    // a pipe: the same command goes on, with a command word next
                    command_next = true;
                    at += if rest.starts_with(b"|&") { 2 } else { 1 };
                } else {
                    // a lone `&`: a redirection (`2>&1`, `&>file`) or a job put in the
                    // background, which this leaves on its line
                    let redirection =
                        (at > 0 && matches!(bytes[at - 1], b'>' | b'<')) || rest.starts_with(b"&>");
                    command_next = !redirection;
                    at += 1;
                }
            }
            _ => at += 1,
        }
    }
    if at > bytes.len() {
        // a backslash was the last byte
        return None;
    }
    pieces.push((start, bytes.len(), bytes.len()));
    let mut steps = Vec::new();
    for (start, end, operator_end) in pieces {
        let text = line[start..end].trim_start();
        let text_end = end - (text.len() - text.trim_end().len());
        let text = text.trim_end();
        let operator = match &line[text_end..operator_end] {
            blank if blank.trim().is_empty() => "",
            operator => operator,
        };
        if text.is_empty() {
            if !operator.is_empty() {
                // an operator with no command before it
                return None;
            }
            continue;
        }
        steps.push(Step { text, operator });
    }
    (!steps.is_empty()).then_some(steps)
}

/// The unquoted word at `at`, to the next blank or operator character.
fn bare_word(bytes: &[u8], at: usize) -> &[u8] {
    let end = bytes[at..]
        .iter()
        .position(|b| b.is_ascii_whitespace() || b";&|()<>".contains(b))
        .map_or(bytes.len(), |end| at + end);
    &bytes[at..end]
}

/// Past `'…'`, which has no escapes.
fn single_quoted(bytes: &[u8], at: usize) -> Option<usize> {
    let end = bytes[at + 1..].iter().position(|b| *b == b'\'')?;
    Some(at + 1 + end + 1)
}

/// Past text that ends at an unescaped `close` (`$'…'` and `` `…` ``).
fn escaped_until(bytes: &[u8], mut at: usize, close: u8) -> Option<usize> {
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            byte if byte == close => return Some(at + 1),
            _ => at += 1,
        }
    }
    None
}

fn backquoted(bytes: &[u8], at: usize) -> Option<usize> {
    escaped_until(bytes, at + 1, b'`')
}

/// Past `"…"`, with the substitutions inside it.
fn double_quoted(bytes: &[u8], at: usize) -> Option<usize> {
    let mut at = at + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return Some(at + 1),
            b'`' => at = backquoted(bytes, at)?,
            b'$' => at = dollar(bytes, at, true)?,
            _ => at += 1,
        }
    }
    None
}

/// Past what a `$` starts: `$(…)`, `${…}`, `$'…'` (outside double quotes), or just the `$`.
fn dollar(bytes: &[u8], at: usize, quoted: bool) -> Option<usize> {
    match bytes.get(at + 1) {
        Some(b'(') => parenthesized(bytes, at + 1),
        Some(b'{') => braced(bytes, at + 1, quoted),
        Some(b'\'') if !quoted => escaped_until(bytes, at + 2, b'\''),
        _ => Some(at + 1),
    }
}

/// Past `(…)` from its `(`: a subshell, a command or arithmetic substitution, an array.
fn parenthesized(bytes: &[u8], at: usize) -> Option<usize> {
    let mut at = at + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'\'' => at = single_quoted(bytes, at)?,
            b'"' => at = double_quoted(bytes, at)?,
            b'`' => at = backquoted(bytes, at)?,
            b'$' => at = dollar(bytes, at, false)?,
            b'(' => at = parenthesized(bytes, at)?,
            b')' => return Some(at + 1),
            _ => at += 1,
        }
    }
    None
}

/// Past `{…}` of a `${…}` from its `{`; `quoted` inside double quotes, where `'` is itself.
fn braced(bytes: &[u8], at: usize, quoted: bool) -> Option<usize> {
    let mut at = at + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'\'' if !quoted => at = single_quoted(bytes, at)?,
            b'"' => at = double_quoted(bytes, at)?,
            b'`' => at = backquoted(bytes, at)?,
            b'$' => at = dollar(bytes, at, quoted)?,
            b'}' => return Some(at + 1),
            _ => at += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str) -> Option<Vec<String>> {
        steps(line).map(|steps| {
            steps
                .iter()
                .map(|step| format!("{}{}", step.text, step.operator))
                .collect()
        })
    }

    fn lines(parts: &[&str]) -> Option<Vec<String>> {
        Some(parts.iter().map(|part| (*part).to_owned()).collect())
    }

    #[test]
    fn commands_end_where_the_shell_ends_them() {
        assert_eq!(
            texts("sed -n 1,2p a && sed -n 3,4p b"),
            lines(&["sed -n 1,2p a &&", "sed -n 3,4p b"])
        );
        assert_eq!(
            texts("git for-each-ref | head -20; echo \"--- remotes:\"; git remote -v"),
            lines(&[
                "git for-each-ref | head -20;",
                "echo \"--- remotes:\";",
                "git remote -v"
            ])
        );
        assert_eq!(
            texts("make || echo failed"),
            lines(&["make ||", "echo failed"])
        );
        assert_eq!(
            texts("pwd\nls -la\n\ngit status"),
            lines(&["pwd", "ls -la", "git status"])
        );
        // an operator at the end of a line carries on to the next
        assert_eq!(
            texts("cargo build &&\n  cargo test"),
            lines(&["cargo build &&", "cargo test"])
        );
        assert_eq!(texts("ls;"), lines(&["ls;"]));
        assert_eq!(texts("a&&b ;c"), lines(&["a&&", "b ;", "c"]));
        assert_eq!(texts("  ls -la  "), lines(&["ls -la"]));
    }

    #[test]
    fn separators_inside_quotes_and_substitutions_are_text() {
        for whole in [
            "sed -i 's/a/b/; s/c/d/' file",
            "sed -n '300,322p;680,735p' docs/protocol.md",
            "git commit -qm \"Spinner; timer && more\"",
            "grep -n 'mime.starts_with(\"image/\")' a.rs | head",
            "echo \"tags: $(git tag | wc -l; echo \"x;y\")\"",
            "echo $(ls; pwd)",
            "echo `ls; pwd`",
            "find . -name '*.orig' -exec rm {} \\;",
            "echo $'it\\'s; fine'",
            "echo \"${name:-a;b}\" ${other:-'c;d'}",
            "(cd crates; ls)",
            "echo $((1 + 2))",
            "cargo test 2>&1 | tail -5",
            "sleep 5 & wait",
            "echo a \\; b",
            "ls # list; not a command",
            "diff <(ls a; ls b) c",
            "cat <<< 'here; string'",
        ] {
            assert_eq!(texts(whole), lines(&[whole]), "{whole}");
        }
        assert_eq!(
            texts("grep \"a;b\" f; sed 's/;/,/' g"),
            lines(&["grep \"a;b\" f;", "sed 's/;/,/' g"])
        );
        assert_eq!(
            texts("ls # one; two\npwd"),
            lines(&["ls # one; two", "pwd"])
        );
    }

    #[test]
    fn what_is_not_understood_is_not_divided() {
        for refused in [
            "python3 - <<'PY'\nprint(1); print(2)\nPY",
            "cat > f <<EOF\na; b\nEOF",
            "for f in a b; do sed -n 1p $f; done",
            "ls; for f in a b; do echo $f; done",
            "if test -f a; then cat a; fi",
            "while read x; do echo $x; done < f",
            "ls | while read x; do echo $x; done",
            "case $x in a) ls;; esac",
            "{ ls; pwd; } > out",
            "f() { ls; pwd; }; f",
            "[[ -f a && -f b ]] && cat a",
            "echo 'unclosed; quote",
            "echo \"unclosed; quote",
            "echo $(ls; pwd",
            "ls; ) pwd",
            "ls; ; pwd",
            "&& ls",
            "ls;; pwd",
            "time for x in a; do ls; done",
            "ls \\",
            "",
            "   ",
        ] {
            assert_eq!(steps(refused), None, "{refused}");
        }
    }

    /// Whatever the line is, shell or not: every step is a slice of it, the slices come in
    /// order and never overlap, and nothing but blanks is left between them.
    #[test]
    fn steps_are_the_line_itself_whatever_the_line() {
        fn check(line: &str) {
            let Some(steps) = steps(line) else { return };
            let base = line.as_ptr() as usize;
            let mut at = 0;
            let mut joined = String::new();
            for step in &steps {
                for part in [step.text, step.operator] {
                    if part.is_empty() {
                        continue;
                    }
                    let from = part.as_ptr() as usize - base;
                    assert!(from >= at, "{line:?}: {part:?} out of order");
                    assert!(line[at..from].trim().is_empty(), "{line:?}: text dropped");
                    at = from + part.len();
                    joined.push_str(part);
                }
                assert!(!step.text.is_empty(), "{line:?}: an empty step");
            }
            assert!(
                line[at..].trim().is_empty(),
                "{line:?}: text dropped at the end"
            );
            let dense =
                |text: &str| -> String { text.chars().filter(|c| !c.is_whitespace()).collect() };
            assert_eq!(dense(&joined), dense(line));
        }
        // lines made of the characters that matter to a shell, and a few that do not
        let alphabet: Vec<char> = "ab ;;&&||\n\t'\"`$(){}<>#\\!é→=-".chars().collect();
        let mut seed: u64 = 0x5EED_1E55;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..20_000 {
            let line: String = (0..next() % 24)
                .map(|_| alphabet[next() % alphabet.len()])
                .collect();
            check(&line);
        }
        for line in [
            "Run the tests; then report",
            "List files in current directory",
            "sed -n 1,2p a && sed -n 3,4p b ; echo ----; grep x \"a;b\" | head",
        ] {
            check(line);
        }
    }
}
