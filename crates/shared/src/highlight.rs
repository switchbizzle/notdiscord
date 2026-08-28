//! Syntax highlighting for fenced code blocks.
//!
//! Hand-rolled on purpose. The obvious answer is syntect, but it carries a
//! regex engine and a pile of TextMate grammars into a client that already
//! ships 37MB — and the web app would pay for it twice, in wasm. What people
//! actually paste into a chat window is twenty lines of one language, where
//! the whole visual win is comments, strings, numbers, and keywords being
//! four different colours. That is a lexer, not a parser, and it fits here.
//!
//! Both clients call this, so a snippet looks the same on the phone as it
//! does on the desktop.

/// What a run of characters is, for colouring purposes. Deliberately coarse:
/// finer distinctions need a real parser and wouldn't survive the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    Plain,
    Comment,
    Str,
    Num,
    Keyword,
    /// Types and constants — the things a reader scans for after keywords.
    Type,
}

impl Tok {
    /// The CSS class the clients style. Kept short; these repeat a lot.
    pub fn class(self) -> &'static str {
        match self {
            Tok::Plain => "t-p",
            Tok::Comment => "t-c",
            Tok::Str => "t-s",
            Tok::Num => "t-n",
            Tok::Keyword => "t-k",
            Tok::Type => "t-t",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub kind: Tok,
    pub text: String,
}

struct Syntax {
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// Quote characters that open a string.
    quotes: &'static [char],
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    /// Rust's `'a` is a lifetime, not a character literal, and treating it as
    /// one swallows the rest of the line.
    lifetimes: bool,
}

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "type", "unsafe", "use",
    "where", "while", "yield",
];
const RUST_TY: &[&str] = &[
    "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8", "u16",
    "u32", "u64", "u128", "usize", "String", "Vec", "Option", "Result", "Some", "None", "Ok",
    "Err", "Box", "true", "false",
];

const PY_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];
const PY_TY: &[&str] =
    &["True", "False", "None", "self", "bool", "bytes", "dict", "float", "int", "list", "set", "str", "tuple"];

const JS_KW: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger",
    "default", "delete", "do", "else", "export", "extends", "finally", "for", "from", "function",
    "if", "import", "in", "instanceof", "interface", "let", "new", "of", "return", "static",
    "switch", "this", "throw", "try", "type", "typeof", "var", "void", "while", "yield",
];
const JS_TY: &[&str] = &[
    "Array", "Boolean", "Error", "JSON", "Map", "Math", "Number", "Object", "Promise", "Set",
    "String", "console", "document", "false", "null", "true", "undefined", "window",
];

const C_KW: &[&str] = &[
    "break", "case", "catch", "class", "const", "continue", "default", "defer", "do", "else",
    "enum", "extends", "extern", "final", "finally", "for", "func", "go", "goto", "if",
    "implements", "import", "interface", "namespace", "new", "package", "private", "protected",
    "public", "range", "return", "sizeof", "static", "struct", "switch", "template", "throw",
    "try", "typedef", "union", "using", "var", "virtual", "void", "while",
];
const C_TY: &[&str] = &[
    "auto", "bool", "byte", "char", "double", "error", "false", "float", "int", "int8", "int16",
    "int32", "int64", "long", "map", "nil", "null", "nullptr", "rune", "short", "signed", "size_t",
    "string", "true", "uint", "unsigned",
];

const SH_KW: &[&str] = &[
    "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if", "in",
    "local", "return", "then", "until", "while",
];
const SH_TY: &[&str] = &["cd", "echo", "exit", "false", "set", "source", "true"];

const SQL_KW: &[&str] = &[
    "ALTER", "AND", "AS", "ASC", "BY", "CREATE", "DELETE", "DESC", "DISTINCT", "DROP", "EXISTS",
    "FROM", "GROUP", "HAVING", "IN", "INDEX", "INNER", "INSERT", "INTO", "JOIN", "LEFT", "LIKE",
    "LIMIT", "NOT", "ON", "OR", "ORDER", "OUTER", "SELECT", "SET", "TABLE", "UPDATE", "VALUES",
    "WHERE", "WITH",
];
const SQL_TY: &[&str] =
    &["BLOB", "BOOLEAN", "INTEGER", "NULL", "PRIMARY", "REAL", "TEXT", "UNIQUE", "KEY"];

const JSON_TY: &[&str] = &["true", "false", "null"];

const TOML_KW: &[&str] = &[];

fn syntax_for(lang: &str) -> Option<Syntax> {
    let lang = lang.trim().to_ascii_lowercase();
    let lang = lang.split_whitespace().next().unwrap_or("");
    Some(match lang {
        "rust" | "rs" => Syntax {
            line_comments: &["//"],
            block_comment: Some(("/*", "*/")),
            quotes: &['"', '\''],
            keywords: RUST_KW,
            types: RUST_TY,
            lifetimes: true,
        },
        "python" | "py" => Syntax {
            line_comments: &["#"],
            block_comment: None,
            quotes: &['"', '\''],
            keywords: PY_KW,
            types: PY_TY,
            lifetimes: false,
        },
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" => Syntax {
            line_comments: &["//"],
            block_comment: Some(("/*", "*/")),
            quotes: &['"', '\'', '`'],
            keywords: JS_KW,
            types: JS_TY,
            lifetimes: false,
        },
        "c" | "cpp" | "c++" | "cs" | "csharp" | "java" | "go" | "kotlin" | "swift" | "php" => {
            Syntax {
                line_comments: &["//"],
                block_comment: Some(("/*", "*/")),
                quotes: &['"', '\''],
                keywords: C_KW,
                types: C_TY,
                lifetimes: false,
            }
        }
        "sh" | "bash" | "zsh" | "shell" | "console" => Syntax {
            line_comments: &["#"],
            block_comment: None,
            quotes: &['"', '\''],
            keywords: SH_KW,
            types: SH_TY,
            lifetimes: false,
        },
        "sql" => Syntax {
            line_comments: &["--"],
            block_comment: Some(("/*", "*/")),
            quotes: &['"', '\''],
            keywords: SQL_KW,
            types: SQL_TY,
            lifetimes: false,
        },
        "json" => Syntax {
            line_comments: &[],
            block_comment: None,
            quotes: &['"'],
            keywords: &[],
            types: JSON_TY,
            lifetimes: false,
        },
        "toml" | "ini" | "yaml" | "yml" => Syntax {
            line_comments: &["#"],
            block_comment: None,
            quotes: &['"', '\''],
            keywords: TOML_KW,
            types: JSON_TY,
            lifetimes: false,
        },
        "css" | "scss" => Syntax {
            line_comments: &["//"],
            block_comment: Some(("/*", "*/")),
            quotes: &['"', '\''],
            keywords: &[],
            types: &[],
            lifetimes: false,
        },
        _ => return None,
    })
}

/// True for a language we can colour. The clients don't need it — an unknown
/// language just comes back as one plain span — but it's cheap to ask.
pub fn is_supported(lang: &str) -> bool {
    syntax_for(lang).is_some()
}

/// Split `code` into coloured runs. An unknown language, or one we have no
/// rules for, comes back as a single plain span, which renders exactly as it
/// did before highlighting existed.
pub fn highlight(lang: &str, code: &str) -> Vec<Span> {
    let Some(syntax) = syntax_for(lang) else {
        return vec![Span { kind: Tok::Plain, text: code.to_owned() }];
    };

    let chars: Vec<char> = code.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    // Runs of Plain are common and boring; batch them into one span.
    macro_rules! flush {
        () => {
            if !plain.is_empty() {
                spans.push(Span { kind: Tok::Plain, text: std::mem::take(&mut plain) });
            }
        };
    }

    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();

        // Comments first: everything inside one is a comment, including
        // quotes that would otherwise open a string.
        if let Some(marker) = syntax.line_comments.iter().find(|m| rest.starts_with(**m)) {
            let end = chars[i..].iter().position(|c| *c == '\n').map_or(chars.len(), |p| i + p);
            flush!();
            spans.push(Span { kind: Tok::Comment, text: chars[i..end].iter().collect() });
            i = end;
            let _ = marker;
            continue;
        }
        if let Some((open, close)) = syntax.block_comment {
            if rest.starts_with(open) {
                let after = i + open.chars().count();
                let end = find_from(&chars, after, close)
                    .map(|p| p + close.chars().count())
                    .unwrap_or(chars.len());
                flush!();
                spans.push(Span { kind: Tok::Comment, text: chars[i..end].iter().collect() });
                i = end;
                continue;
            }
        }

        let c = chars[i];

        // A Rust lifetime looks exactly like the start of a character
        // literal. Treat `'` as a quote only when a closing one turns up
        // close by on the same line.
        let lifetime = syntax.lifetimes && c == '\'' && !is_char_literal(&chars, i);
        if syntax.quotes.contains(&c) && !lifetime {
            let end = string_end(&chars, i, c);
            flush!();
            spans.push(Span { kind: Tok::Str, text: chars[i..end].iter().collect() });
            i = end;
            continue;
        }

        if c.is_ascii_digit() {
            let mut end = i;
            while end < chars.len() && is_number_char(chars[end]) {
                end += 1;
            }
            flush!();
            spans.push(Span { kind: Tok::Num, text: chars[i..end].iter().collect() });
            i = end;
            continue;
        }

        if is_word_start(c) {
            let mut end = i;
            while end < chars.len() && is_word_char(chars[end]) {
                end += 1;
            }
            let word: String = chars[i..end].iter().collect();
            let kind = classify(&word, &syntax);
            if kind == Tok::Plain {
                plain.push_str(&word);
            } else {
                flush!();
                spans.push(Span { kind, text: word });
            }
            i = end;
            continue;
        }

        plain.push(c);
        i += 1;
    }
    flush!();
    spans
}

fn classify(word: &str, syntax: &Syntax) -> Tok {
    if syntax.keywords.iter().any(|k| *k == word) {
        return Tok::Keyword;
    }
    if syntax.types.iter().any(|t| *t == word) {
        return Tok::Type;
    }
    // SQL is conventionally shouted, so match its keywords either way.
    let upper = word.to_ascii_uppercase();
    if syntax.line_comments.contains(&"--") && syntax.keywords.iter().any(|k| *k == upper) {
        return Tok::Keyword;
    }
    if syntax.line_comments.contains(&"--") && syntax.types.iter().any(|t| *t == upper) {
        return Tok::Type;
    }
    Tok::Plain
}

/// Where the string starting at `open` ends, one past its closing quote.
/// An unterminated string runs to the end of the line, not the end of the
/// file — someone's stray quote shouldn't paint the rest of the snippet.
fn string_end(chars: &[char], open: usize, quote: char) -> usize {
    let mut i = open + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '\n' if quote != '`' => return i,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    chars.len()
}

/// Is the `'` at `open` a character literal rather than a lifetime? Only the
/// exact shapes count: a single character in quotes, or an escape. The looser
/// question — "does another quote turn up soon?" — answers yes to
/// `fn f<'a>(s: &'a str)`, where the next quote is the *next lifetime*, and
/// then half the signature is a string.
fn is_char_literal(chars: &[char], open: usize) -> bool {
    match chars.get(open + 1) {
        // An escape runs a little longer, but still closes on the same line.
        Some('\\') => {
            let limit = (open + 12).min(chars.len());
            let tail = &chars[open + 2..limit];
            tail.contains(&'\'') && !tail.contains(&'\n')
        }
        Some(_) => chars.get(open + 2) == Some(&'\''),
        None => false,
    }
}

fn find_from(chars: &[char], start: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    (start..chars.len().saturating_sub(needle.len() - 1))
        .find(|&i| chars[i..i + needle.len()] == needle[..])
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$' || c == '@'
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn is_number_char(c: char) -> bool {
    c.is_ascii_hexdigit() || matches!(c, '.' | '_' | 'x' | 'X' | 'b' | 'o' | '+' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text must survive exactly — highlighting is only allowed to
    /// change colours, never a single character of what was pasted.
    fn roundtrip(lang: &str, code: &str) -> Vec<Span> {
        let spans = highlight(lang, code);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, code, "highlighting {lang} changed the text");
        spans
    }

    fn kinds_of(spans: &[Span], text: &str) -> Vec<Tok> {
        spans.iter().filter(|s| s.text.contains(text)).map(|s| s.kind).collect()
    }

    #[test]
    fn colours_the_four_things_that_matter() {
        let spans = roundtrip("rust", "// count\nlet x = 42; // and\nlet s = \"hi\";");
        assert_eq!(kinds_of(&spans, "// count"), vec![Tok::Comment]);
        assert_eq!(kinds_of(&spans, "let"), vec![Tok::Keyword, Tok::Keyword]);
        assert_eq!(kinds_of(&spans, "42"), vec![Tok::Num]);
        assert_eq!(kinds_of(&spans, "\"hi\""), vec![Tok::Str]);
    }

    #[test]
    fn a_lifetime_is_not_a_character_literal() {
        // The bug this guards: treating `'a` as an opening quote swallows the
        // rest of the line into a string.
        let spans = roundtrip("rust", "fn f<'a>(s: &'a str) -> &'a str { s }");
        assert!(!spans.iter().any(|s| s.kind == Tok::Str), "{spans:?}");
        assert_eq!(kinds_of(&spans, "str"), vec![Tok::Type, Tok::Type]);
        // A real character literal still colours.
        let spans = roundtrip("rust", "let c = 'x';");
        assert_eq!(kinds_of(&spans, "'x'"), vec![Tok::Str]);
    }

    #[test]
    fn comments_win_over_everything_inside_them() {
        // A quote or a keyword inside a comment must not escape it.
        let spans = roundtrip("python", "# don't let \"this\" break def\nx = 1");
        assert_eq!(spans[0].kind, Tok::Comment);
        assert!(spans[0].text.ends_with("break def"));
        assert_eq!(kinds_of(&spans, "1"), vec![Tok::Num]);

        // And a comment marker inside a string is just text.
        let spans = roundtrip("python", "url = \"http://x#y\"\n# real");
        assert_eq!(kinds_of(&spans, "\"http://x#y\""), vec![Tok::Str]);
    }

    #[test]
    fn block_comments_and_unterminated_things_terminate() {
        let spans = roundtrip("js", "/* a\n   b */ let x = 1;");
        assert_eq!(spans[0].kind, Tok::Comment);
        assert!(spans[0].text.contains('\n'));

        // Unterminated block comment: everything after it, and no panic.
        let spans = roundtrip("js", "let x = 1; /* forever");
        assert_eq!(spans.last().unwrap().kind, Tok::Comment);

        // An unterminated string stops at the newline, so one stray quote
        // can't paint the rest of the snippet.
        let spans = roundtrip("js", "let a = \"oops\nlet b = 2;");
        let str_span = spans.iter().find(|s| s.kind == Tok::Str).unwrap();
        assert!(!str_span.text.contains('\n'), "{str_span:?}");
        assert_eq!(kinds_of(&spans, "2"), vec![Tok::Num]);
    }

    #[test]
    fn sql_is_shouted_or_not() {
        let upper = roundtrip("sql", "SELECT * FROM users WHERE id = 1");
        let lower = roundtrip("sql", "select * from users where id = 1");
        assert_eq!(kinds_of(&upper, "SELECT"), vec![Tok::Keyword]);
        assert_eq!(kinds_of(&lower, "select"), vec![Tok::Keyword]);
        assert_eq!(kinds_of(&lower, "1"), vec![Tok::Num]);
    }

    #[test]
    fn an_unknown_language_is_left_alone() {
        for lang in ["", "brainfuck", "text", "  "] {
            let spans = roundtrip(lang, "anything at all\n  with lines");
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].kind, Tok::Plain);
        }
        // The fence can carry extra words ("rust,ignore" style is common).
        assert!(is_supported("rust"));
        assert!(is_supported("RUST"));
        assert!(!is_supported("nonsense"));
    }

    #[test]
    fn every_shape_of_number_and_no_panics() {
        let spans = roundtrip("rust", "let a = 0xFF; let b = 1_000; let c = 1.5e3;");
        assert_eq!(kinds_of(&spans, "0xFF"), vec![Tok::Num]);
        assert_eq!(kinds_of(&spans, "1_000"), vec![Tok::Num]);
        // Unicode and emoji in code must not split a char boundary.
        for code in ["let s = \"héllo 🎧\";", "# ünïcödé", "'", "\"", "/*", "//"] {
            roundtrip("rust", code);
            roundtrip("python", code);
            roundtrip("json", code);
        }
    }
}
