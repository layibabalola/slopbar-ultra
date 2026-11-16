//! Source guards: tests that read code for what no other test can see (the limit each step of an
//! update runs within, where an install takes its turn, what the exit cleanup bounds, how every
//! wait for the refresh lock ends). They read tokens, not layout (`tokens`), so rustfmt, a call
//! broken over lines or a block indented anew reads the same; and only code: a needle spelled
//! inside a string or character literal is not found there. rustfmt also puts braces around a
//! closure's body when it breaks one over lines, so a call that takes a closure is read argument by
//! argument (`calls`, `args`, `closure_body`), never as one needle, and the body it reads is compared
//! by equality (see `closure_body`). A rename does not read the same, and then the failure names the
//! text that went and the test to update (`lost`, `update`).

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `source` as a guard reads it, code and needles alike: no comments; whitespace only where it
/// keeps two words apart, a word from a quote or `#` after it (`break 'a '}'`, or `r "a\"b"`
/// in a macro, which run together would read as the literal `'a'` or the raw string `r"a\"`),
/// or `/` from a `/` or `*` after it, as one space; and no comma right before a closing
/// bracket, which rustfmt adds when it breaks a call over lines. String and character literals
/// are kept as written.
pub fn tokens(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut gap = false;
    let mut i = 0;
    while let Some(c) = source[i..].chars().next() {
        let rest = &source[i..];
        if rest.starts_with("//") {
            i += rest.find('\n').unwrap_or(rest.len());
            gap = true;
        } else if rest.starts_with("/*") {
            i += block_comment(rest);
            gap = true;
        } else if c.is_whitespace() {
            i += c.len_utf8();
            gap = true;
        } else {
            let end = literal_end(source, i).unwrap_or(i + c.len_utf8());
            if matches!(c, ')' | ']' | '}') && out.ends_with(',') {
                out.pop();
            }
            let last = out.chars().next_back();
            if gap && (((word(c) || matches!(c, '\'' | '"' | '#')) && last.is_some_and(word)) || (last == Some('/') && matches!(c, '/' | '*'))) {
                out.push(' ');
            }
            out.push_str(&source[i..end]);
            gap = false;
            i = end;
        }
    }
    out
}

/// How long the block comment that `rest` starts with is, the comments nested in it included.
fn block_comment(rest: &str) -> usize {
    let mut depth = 0usize;
    let mut i = 0;
    while let Some(c) = rest[i..].chars().next() {
        if rest[i..].starts_with("/*") {
            depth += 1;
            i += 2;
        } else if rest[i..].starts_with("*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += c.len_utf8();
        }
    }
    rest.len()
}

/// Where the string or character literal that starts at byte `at` of `s` ends, if one starts
/// there: `"…"`, `r#"…"#` (and `b`, `c` before either), `'…'`. A lifetime or a label is none.
fn literal_end(s: &str, at: usize) -> Option<usize> {
    let rest = &s[at..];
    if rest.starts_with('"') {
        let mut escaped = false;
        for (i, c) in rest.char_indices().skip(1) {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => return Some(at + i + 1),
                _ => {}
            }
        }
        return Some(s.len());
    }
    if rest.starts_with('r') && raw_prefix(s, at) {
        let hashes = rest[1..].len() - rest[1..].trim_start_matches('#').len();
        let open = 1 + hashes;
        if !rest[open..].starts_with('"') {
            return None;
        }
        let close = format!("\"{}", "#".repeat(hashes));
        return Some(rest[open + 1..].find(&close).map_or(s.len(), |e| at + open + 1 + e + close.len()));
    }
    if rest.starts_with('\'') {
        let mut chars = rest.char_indices().skip(1);
        let (_, first) = chars.next()?;
        if first == '\\' {
            chars.next()?;
            return chars.find(|&(_, c)| c == '\'').map(|(i, _)| at + i + 1);
        }
        let (i, second) = chars.next()?;
        return (first != '\'' && second == '\'').then_some(at + i + 1);
    }
    None
}

/// Whether an `r` at byte `at` of `s` can start a raw string: it starts a token, alone or after
/// a `b` or `c` that does.
fn raw_prefix(s: &str, at: usize) -> bool {
    let mut before = s[..at].chars().rev();
    match before.next() {
        None => true,
        Some('b' | 'c') => !before.next().is_some_and(word),
        Some(c) => !word(c),
    }
}

/// The characters of `code` from byte `from` on that are not inside a literal, with their bytes.
fn outside_literals(code: &str, from: usize) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut i = from;
    std::iter::from_fn(move || loop {
        let c = code[i..].chars().next()?;
        if let Some(end) = literal_end(code, i) {
            i = end;
            continue;
        }
        i += c.len_utf8();
        return Some((i - c.len_utf8(), c));
    })
}

/// The bracket that closes the one at byte `open` of `code`.
fn closing(code: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, c) in outside_literals(code, open) {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// The literals in `code`, in order: where each starts and where it ends.
fn literals(code: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while let Some(c) = code[i..].chars().next() {
        match literal_end(code, i) {
            Some(end) => {
                spans.push((i, end));
                i = end;
            }
            None => i += c.len_utf8(),
        }
    }
    spans
}

/// Where `pattern` occurs in `code` (tokens), but never inside a literal: text spelled in a
/// string or character literal is not code. A match that starts at a literal's opening quote
/// is the literal itself, and counts.
fn occurrences(code: &str, pattern: &str) -> Vec<usize> {
    let spans = literals(code);
    let inside = |at: usize| {
        let k = spans.partition_point(|&(start, _)| start < at);
        k > 0 && spans[k - 1].1 > at
    };
    code.match_indices(pattern).map(|(at, _)| at).filter(|&at| !inside(at)).collect()
}

/// Where `needle`, read as tokens itself, starts in `code` (tokens): every place, in order.
pub fn places(code: &str, needle: &str) -> Vec<usize> {
    occurrences(code, &tokens(needle))
}

/// Where `needle` first starts in `code` (tokens), read as tokens itself.
pub fn find(code: &str, needle: &str) -> Option<usize> {
    places(code, needle).first().copied()
}

/// Whether `code` (tokens) has `needle`, read as tokens itself.
pub fn has(code: &str, needle: &str) -> bool {
    find(code, needle).is_some()
}

/// How often `code` (tokens) has `needle`, read as tokens itself.
pub fn count(code: &str, needle: &str) -> usize {
    places(code, needle).len()
}

/// The function (or arm, or block) whose code starts `signature`, through the brace that closes
/// it; a failure that says what to update in `test` when `file` (tokens: `code`) has none.
pub fn function<'a>(code: &'a str, file: &str, signature: &str, test: &str) -> &'a str {
    let start = find(code, signature).unwrap_or_else(|| panic!("{}", lost(file, signature, test)));
    let open = outside_literals(code, start).find(|&(_, c)| c == '{').map(|(at, _)| at);
    let close = open.and_then(|open| closing(code, open)).unwrap_or_else(|| panic!("`{signature}` in {file} has no body that closes, as {test} reads it"));
    &code[start..=close]
}

/// Every call of `name` in `code` (tokens), each from its opening parenthesis to its closing one;
/// a call spelled in a literal is none. A call that never closes fails, naming `test` to update.
pub fn calls<'a>(code: &'a str, name: &str, test: &str) -> Vec<&'a str> {
    let name = tokens(name);
    let named = |at: usize| name.starts_with(word) && code[..at].chars().next_back().is_some_and(word);
    occurrences(code, &name)
        .into_iter()
        .map(|at| at + name.len())
        .filter(|&open| code[open..].starts_with('(') && !named(open - name.len()))
        .map(|open| {
            let close = closing(code, open).unwrap_or_else(|| panic!("the call of `{name}` at byte {open} never closes, as {test} reads it. {}", update(test)));
            &code[open..=close]
        })
        .collect()
}

/// Whether `head`, the text of an argument before its first `|`, makes it a closure: nothing, or
/// `move`, `async`, `async move`, or a reference to one (`&`, `&mut`).
fn closure_head(head: &str) -> bool {
    let head = head.strip_prefix('&').map_or(head, |rest| rest.strip_prefix("mut").map_or(rest, str::trim_start));
    let head = head.strip_prefix("async").map_or(head, str::trim_start);
    matches!(head, "" | "move")
}

/// The arguments of `call` (a call's brackets, as `calls` gives them), in order: split at the
/// commas between them, never at one inside a bracket, a literal, a closure's parameters
/// (`|a, b|`) or return type (`|..| -> R<A, B> {`), or a turbofish (`f::<A, B>(..)`). An argument
/// stays as the reader has it (`tokens`).
pub fn args(call: &str) -> Vec<&str> {
    let inner = call.strip_prefix('(').and_then(|c| c.strip_suffix(')')).unwrap_or_else(|| panic!("`{call}` is no call's brackets"));
    let (mut out, mut start, mut depth, mut params) = (Vec::new(), 0, 0usize, false);
    let (mut angle, mut returns, mut last) = (0usize, false, None);
    for (at, c) in outside_literals(inner, 0) {
        match c {
            // The body's brace ends a closure's return type.
            '{' if depth == 0 && returns => {
                returns = false;
                depth += 1;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            // A turbofish's `<` follows `::`; a `>` after `-` is an arrow.
            '<' if depth == 0 && (angle > 0 || last == Some(':')) => angle += 1,
            '>' if depth == 0 && angle > 0 && last != Some('-') => angle -= 1,
            // A closure's parameters open where an argument starts (`closure_head`) and close at the
            // next `|`; a `|` anywhere else is an operator.
            '|' if depth == 0 && angle == 0 && !returns => {
                if params && inner[at + 1..].starts_with("->") {
                    returns = true;
                }
                params = !params && closure_head(&inner[start..at]);
            }
            ',' if depth == 0 && angle == 0 && !params && !returns => {
                out.push(&inner[start..at]);
                start = at + 1;
            }
            _ => {}
        }
        last = Some(c);
    }
    if !inner.is_empty() {
        out.push(&inner[start..]);
    }
    out
}

/// What the closure `arg` returns (`|..| body`, after `move`, `async` or `&` as `closure_head` reads
/// them), without the braces rustfmt puts around a body that holds one expression when it breaks
/// it over lines: `||{f(x)}` and `||f(x)` read the same. The parameters end at the first `|`
/// outside brackets. None when `arg` is no closure.
///
/// A guard compares a body it reads this way by equality, never with `has` or `find`: a body with
/// a return type (`->T{x}`) or a block of statements is read as written, and a needle found inside
/// one would pass a closure that does more than the needle says.
pub fn closure_body(arg: &str) -> Option<&str> {
    let bar = arg.find('|')?;
    if !closure_head(&arg[..bar]) {
        return None;
    }
    let rest = &arg[bar + 1..];
    let mut depth = 0usize;
    let (end, _) = outside_literals(rest, 0).find(|&(_, c)| {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        c == '|' && depth == 0
    })?;
    let body = &rest[end + 1..];
    if body.starts_with('{') && closing(body, 0) == Some(body.len() - 1) {
        let inner = &body[1..body.len() - 1];
        let mut depth = 0usize;
        let statements = outside_literals(inner, 0).any(|(_, c)| {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
            c == ';' && depth == 0
        });
        if !statements {
            return Some(inner);
        }
    }
    Some(body)
}

/// The statement in `code` (tokens) that starts with `start` (read as tokens itself), through the
/// `;` that ends it: never one inside a bracket or a literal. None when `code` has no such start.
pub fn statement<'a>(code: &'a str, start: &str) -> Option<&'a str> {
    let at = find(code, start)?;
    let mut depth = 0usize;
    let (end, _) = outside_literals(code, at).find(|&(_, c)| {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        c == ';' && depth == 0
    })?;
    Some(&code[at..=end])
}

/// How often `name` stands alone in `code`, not inside a longer name or a literal.
pub fn words(code: &str, name: &str) -> usize {
    let named = |c: Option<char>| c.is_some_and(word);
    occurrences(code, name).into_iter().filter(|&at| !named(code[..at].chars().next_back()) && !named(code[at + name.len()..].chars().next())).count()
}

/// A guard's failure when `place` no longer has `needle`.
pub fn lost(place: &str, needle: &str, test: &str) -> String {
    format!("{place} no longer has `{needle}`. {}", update(test))
}

/// What every guard's failure ends with: what to update after a rename that keeps the behaviour.
pub fn update(test: &str) -> String {
    format!("If a rename or a move left what the code does as it was, update the text this guard looks for in {test}. Otherwise the guard caught a change in what the code does.")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST: &str = "source_guard::tests";

    /// The guards read tokens: what rustfmt or a hand changes in layout reads the same; what the
    /// code is does not.
    #[test]
    fn guards_read_tokens_not_layout() {
        let one_line = "update_check::hand_over(steps, update_check::HANDOFF_LIMIT, (turn, refresh), move || update.install(bytes).map_err(|e| describe(&e))).await?;";
        let rustfmt = "update_check::hand_over(\n    steps,\n    update_check::HANDOFF_LIMIT,\n    (turn, refresh),\n    move || update.install(bytes).map_err(|e| describe(&e)),\n)\n.await?;";
        assert_eq!(tokens(rustfmt), tokens(one_line), "a call broken over lines, with rustfmt's trailing comma");
        assert_eq!(tokens("updater(app)?\n            .check()\n            .await"), tokens("updater(app)?.check().await"), "a method chain broken over lines");
        let deferral = "if crate::signin::any_active() {\n        return Ok(Outcome::Deferred(SIGN_IN_OPEN.into()));\n    }";
        assert_eq!(tokens(&deferral.replace('\n', "\n    ")), tokens(deferral), "indented anew");
        assert_eq!(tokens("if  crate::signin::any_active()\n{ return Ok(Outcome::Deferred(SIGN_IN_OPEN.into())); }"), tokens(deferral), "braces and spaces moved");
        assert_eq!(tokens("let  turn =\n match x /* a /* nested */ note */ { // the turn\n Ok(t) => t, }"), "let turn=match x{Ok(t)=>t}", "comments go; words stay apart");
        assert_ne!(tokens("letturn = x"), tokens("let turn = x"), "words are not run together");
        assert_ne!(tokens("(a, b)"), tokens("(a, c)"), "a rename is a change");
        // Literals are kept as written, with what looks like a comment or a bracket inside them.
        let literals = r##"f("a  //  b ) {", '}', '\'', '"', r#"x ) "y" // z"#, b"(", &'a str, 'label: loop {})"##;
        assert_eq!(tokens(literals), r##"f("a  //  b ) {",'}','\'','"',r#"x ) "y" // z"#,b"(",&'a str,'label:loop{})"##);
        assert_eq!(calls(&tokens(literals), "f", TEST), [tokens(literals)[1..].to_string()], "the call ends at its own bracket, past every literal");
        // A raw string ends at its own quote and hashes, past a quote inside it.
        let raw = tokens(r##"f(r#"a"b"#, x)"##);
        assert_eq!(args(calls(&raw, "f", TEST)[0]), [r##"r#"a"b"#"##, "x"], "the raw string is one argument");
        assert_eq!(tokens("a / *b"), "a/ *b", "no comment is made where there was none");
        // A label stays apart from a character literal after it: run together, `'a '}'` would read
        // as the literal `'a'` and a `}` that ends the body early (or, with `'"'`, a string that
        // never ends), and a guard would pass on half a function.
        for literal in ["'}'", "'\"'"] {
            let body = format!("fn f() {{ let _k = 'a: {{ break 'a {literal}; }}; signin::close_all(); }}");
            let code = tokens(&format!("{body}\nfn g() {{ wait(); }}"));
            assert_eq!(function(&code, "the test's code", "fn f(", TEST), tokens(&body), "the body ends at its own brace");
            assert_eq!(count(&code, "signin::close_all"), 1);
        }
        // So does a word from a string or `#` after it: in a macro, `r "a\"b"` run together would
        // read as the raw string `r"a\"`, and its last quote would open a string that hid the code
        // after it.
        let body = r#"fn f() { let _m = stringify!(r "a\"b"); let _s = state.lock().unwrap(); let _q = identity('"'); }"#;
        let code = tokens(&format!("{body}\nfn g() {{ wait(); }}"));
        assert_eq!(function(&code, "the test's code", "fn f(", TEST), tokens(body), "the body ends at its own brace");
        assert!(has(&code, ".lock().unwrap()"), "the code after the string is read as code");
        // Text in a literal is not code: a needle spelled there is not found, though a needle that
        // is the whole literal is.
        let spelled = tokens("fn f() { let s = \"update.download( turn \"; let c = '('; }");
        assert!(!has(&spelled, "update.download("), "a needle spelled in a string");
        assert_eq!((words(&spelled, "turn"), calls(&spelled, "update.download", TEST).len()), (0, 0), "a word, and a call, spelled in a string");
        assert_eq!(find(&spelled, "\"update.download( turn \""), spelled.find('"'), "a whole literal");
        assert_eq!(function(&spelled, "the test's code", "fn f(", TEST), spelled, "a bracket in a literal does not count");
        for source in [include_str!("updates.rs"), include_str!("app.rs"), include_str!("update_check.rs"), include_str!("door.rs"), include_str!("sharing.rs"), include_str!("refresh_wait.rs")] {
            let once = tokens(source);
            assert_eq!(tokens(&once), once, "reading tokens again changes nothing");
        }
    }

    /// A call that takes a closure reads the same whether or not rustfmt broke it over lines, put
    /// braces around the closure's body or added a trailing comma; what the call does does not.
    #[test]
    fn calls_are_read_argument_by_argument() {
        let one_line = tokens("let r = refresh_wait::under(&state.refresh_lock, refresh_wait::LOCK_WAIT, || refresh_locked(app)).await;");
        let rustfmt = tokens("let r = refresh_wait::under(&state.refresh_lock, refresh_wait::LOCK_WAIT, || {\n    refresh_locked(app)\n})\n.await;");
        for code in [&one_line, &rustfmt] {
            let call = calls(code, "refresh_wait::under", TEST);
            assert_eq!(call.len(), 1, "{code}");
            let [lock, limit, refresh] = args(call[0])[..] else { panic!("three arguments: {:?}", args(call[0])) };
            assert_eq!((lock, limit, closure_body(refresh)), ("&state.refresh_lock", "refresh_wait::LOCK_WAIT", Some("refresh_locked(app)")));
        }
        assert_eq!(args(&tokens(r#"(a, |x, y| x | y, move |z| { z }, "b,c", (d, e), [f, g], h(i, j),)"#)),
            ["a", "|x,y|x|y", "move|z|{z}", r#""b,c""#, "(d,e)", "[f,g]", "h(i,j)"], "commas in a closure's parameters, a literal or a bracket");
        assert_eq!(args("()"), Vec::<&str>::new());
        assert_eq!(args(&tokens("(move || f(), |a| a, x)")), ["move||f()", "|a|a", "x"], "`move`, and closures without parameters");
        assert_eq!(closure_body("move|request|{door_reply(h.clone(),request)}"), Some("door_reply(h.clone(),request)"));
        assert_eq!(closure_body("||{let x=f();x}"), Some("{let x=f();x}"), "a block of statements is the body as written");
        assert_eq!(closure_body(r#"||{f(";")}"#), Some(r#"f(";")"#), "a `;` in a literal ends no statement");
        assert_eq!(closure_body("||{f()}.g()"), Some("{f()}.g()"), "braces that end before the body does are the body's own");
        assert_eq!((closure_body("async move{f()}"), closure_body("f(x)"), closure_body("a|b")), (None, None, None), "no closure");
        // A closure that returns a type with a comma in it (app.rs's rclone check), a turbofish,
        // and closures after `&`, `&mut`, `async` and `async move`: each is one argument.
        let rclone = tokens("(move || -> Result<Option<Connect>, &'static str> { let listed = f()?; Ok(None) })");
        assert_eq!(args(&rclone), ["move||->Result<Option<Connect>,&'static str>{let listed=f()?;Ok(None)}"], "a closure's return type");
        assert_eq!(closure_body(args(&rclone)[0]), Some("->Result<Option<Connect>,&'static str>{let listed=f()?;Ok(None)}"), "its body as written, return type and all");
        assert_eq!(args(&tokens("(HashMap::<u8, u8>::new(), m.n::<B, C>(o), x)")), ["HashMap::<u8,u8>::new()", "m.n::<B,C>(o)", "x"], "turbofish");
        // Inside a turbofish, a `<` opens another level and the `>` of an arrow closes none.
        assert_eq!(args(&tokens("(f::<Vec<u8>, B>(x), g::<fn() -> u8, C>(y), z)")), ["f::<Vec<u8>,B>(x)", "g::<fn()->u8,C>(y)", "z"], "a turbofish's own levels");
        let closures = tokens("(&|a, b| a + b, &mut |a, b| a, async |a, b| a, async move |a, b| a, a > b, c)");
        let referenced = args(&closures);
        assert_eq!(referenced, ["&|a,b|a+b", "&mut|a,b|a", "async|a,b|a", "async move|a,b|a", "a>b", "c"], "a closure after `&`, `&mut`, `async`");
        assert_eq!(referenced[..4].iter().map(|arg| closure_body(arg)).collect::<Vec<_>>(), [Some("a+b"), Some("a"), Some("a"), Some("a")]);
        assert_eq!(closure_body(&tokens("|(A(x) | B(x))| x")), Some("x"), "a `|` inside the parameters' brackets");
        let code = tokens("let live = || { f(x) }; let Some(a) = b else { return; }; g();");
        assert_eq!(statement(&code, "let live ="), Some("let live=||{f(x)};"));
        assert_eq!(statement(&code, "let Some(a) = b else {"), Some("let Some(a)=b else{return;};"), "a `;` inside a block ends no statement");
        assert_eq!(statement(&code, "h("), None);
    }
}
