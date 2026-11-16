//! Whether a sign-in is open (`AppState::sign_in_open`): each entry point's sign-in counts, in any
//! window, for as long as it is open. The entry points are read from the code: every function that
//! starts a sign-in (calls `accounts::start_login_observed`), and every function that calls one.

use super::*;
use crate::source_guard::{calls, find, function, has, places, statement, tokens, update};

/// Where each guard's needles are, for its failure (see `source_guard::update`).
const ENTRY: &str = "app::sign_in_open_tests::each_entry_point_s_sign_in_is_open_in_any_window (src-tauri/src/app/sign_in_open_tests.rs)";
const MARK: &str = "app::sign_in_open_tests::every_sign_in_holds_its_mark_until_it_ends (src-tauri/src/app/sign_in_open_tests.rs)";
const HELPER: &str = "app::sign_in_open_tests::the_helper_asks_everything_that_shows_a_sign_in (src-tauri/src/app/sign_in_open_tests.rs)";

/// A sign-in in a private window or the default browser has no fresh profile, so there
/// `signin::any_active` says no.
const NO_PROFILE: bool = false;

/// The functions that start a sign-in, and the entry points that call each.
const OPENERS: [(&str, &[&str]); 3] = [
    // "Add account": Settings' (`add_account`) and an agent's (the door's AddAccount, `door_call`).
    ("start_add_account", &["add_account", "door_call"]),
    // "Sign in all accounts" signing a restored account in again (`run_sign_ins`).
    ("add_expected", &["run_sign_ins"]),
    // Connect monitoring: the user's or an agent's (`connect_monitoring`), "Connect monitoring for
    // all" and "Sign in all accounts" (`run_sign_ins`), and the automatic one (`auto_connect_tick`).
    ("start_monitoring_login", &["connect_monitoring", "run_sign_ins", "auto_connect_tick"]),
];

/// What a sign-in holds while it is open (a reservation or its mark).
type Hold = Box<dyn std::any::Any>;

/// Each thing a function that starts a sign-in takes before it starts, as the code spells it, and
/// the same taken here.
fn holds() -> [(&'static str, fn(&monitoring::Operations, &monitoring::SignIns) -> Hold); 4] {
    [
        ("app.state::<AppState>().sign_ins.open()", |_, sign_ins| Box::new(sign_ins.open())),
        ("app.state::<AppState>().account_operations.begin_key(monitoring::SIGN_IN)", |operations, _| Box::new(operations.begin_key(monitoring::SIGN_IN).unwrap())),
        ("app.state::<AppState>().account_operations.begin(provider, &id)", |operations, _| Box::new(operations.begin(Provider::Claude, "synthetic-id").unwrap())),
        ("app.state::<AppState>().account_operations.begin(provider, \"add-account\")", |operations, _| Box::new(operations.begin(Provider::Claude, "add-account").unwrap())),
    ]
}

/// Every source file of the crate as the guards read it (`tokens`), by its path.
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        let mut paths: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push((path.display().to_string(), tokens(&std::fs::read_to_string(&path).unwrap())));
            }
        }
    }
    let mut out = Vec::new();
    walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
    assert!(out.iter().any(|(path, _)| path.ends_with("app.rs")), "the crate's sources are under {}", env!("CARGO_MANIFEST_DIR"));
    out
}

/// The innermost function in `code` (tokens) whose body holds byte `at`: its name and its code.
fn enclosing<'a>(code: &'a str, at: usize, file: &str) -> (&'a str, &'a str) {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    places(code, "fn")
        .into_iter()
        .filter(|&start| start <= at && code[start + 2..].starts_with(' ') && !code[..start].chars().next_back().is_some_and(word))
        .map(|start| (start, function(&code[start..], file, "fn", MARK)))
        .filter(|(start, body)| at < start + body.len())
        .last()
        .map(|(_, body)| (body[3..].split(['(', '<']).next().unwrap(), body))
        .unwrap_or_else(|| panic!("the call at byte {at} of {file} is in no function. {}", update(MARK)))
}

/// Each function in `sources` that calls `name` (its definition is no call), once: its file, its
/// name and its code.
fn callers<'a>(sources: &'a [(String, String)], name: &str) -> Vec<(&'a str, &'a str, &'a str)> {
    let mut found: Vec<(&str, &str, &str)> = Vec::new();
    for (file, code) in sources {
        for call in calls(code, name, MARK) {
            let at = call.as_ptr() as usize - code.as_ptr() as usize - name.len();
            if code[..at].ends_with("fn ") {
                continue;
            }
            let (caller, body) = enclosing(code, at, file);
            if !found.iter().any(|(f, c, _)| *f == file.as_str() && *c == caller) {
                found.push((file.as_str(), caller, body));
            }
        }
    }
    found
}

fn names<'a>(found: &[(&str, &'a str, &str)]) -> Vec<&'a str> {
    let mut names: Vec<&str> = found.iter().map(|(_, name, _)| *name).collect();
    names.sort_unstable();
    names
}

fn sorted<'a>(names: &[&'a str]) -> Vec<&'a str> {
    let mut names = names.to_vec();
    names.sort_unstable();
    names
}

/// A sign-in started from any entry point counts as open in a private window or the default
/// browser, where no fresh profile shows it, until it ends: the code's own holds, taken here.
#[test]
fn each_entry_point_s_sign_in_is_open_in_any_window() {
    let sources = sources();
    let openers = callers(&sources, "start_login_observed");
    assert_eq!(names(&openers), sorted(&OPENERS.map(|(opener, _)| opener)),
        "the functions that start a sign-in (call accounts::start_login_observed). A new one holds the mark (see {MARK}) and is listed in OPENERS. {}", update(ENTRY));
    for (file, opener, body) in &openers {
        let before = &body[..find(body, "start_login_observed(").unwrap()];
        let replay: Vec<_> = holds().into_iter().flat_map(|(spelled, take)| places(before, spelled).into_iter().map(move |_| take)).collect();
        let taken = places(before, ".account_operations.").len() + places(before, ".sign_ins.").len();
        assert_eq!(replay.len(), taken, "{opener} in {file} takes something this test does not know before its sign-in starts: add it to holds(). {}\n{before}", update(ENTRY));
        let (_, entry_points) = OPENERS.iter().find(|(o, _)| o == opener).unwrap();
        assert_eq!(names(&callers(&sources, opener)), sorted(entry_points),
            "the entry points that start a sign-in through {opener}. A new one is listed in OPENERS. {}", update(ENTRY));
        for entry_point in entry_points.iter() {
            let (operations, sign_ins) = (monitoring::Operations::default(), monitoring::SignIns::default());
            let held: Vec<Hold> = replay.iter().map(|take| take(&operations, &sign_ins)).collect();
            assert!(any_sign_in_open(NO_PROFILE, &operations, &sign_ins),
                "a sign-in from {entry_point} (through {opener}) is open, though no fresh profile shows it. {}\n{before}", update(ENTRY));
            drop(held);
            assert!(!any_sign_in_open(NO_PROFILE, &operations, &sign_ins), "a sign-in from {entry_point} is closed once it ends");
        }
    }
}

/// Every function that starts a sign-in takes its mark (`.sign_ins.open()`) in its own body,
/// whatever happens, before anything of the sign-in starts; binds it to a name; and lets go of it
/// only once the sign-in has been waited for (and what it signed in kept), or as the function or its
/// task ends however it ends.
#[test]
fn every_sign_in_holds_its_mark_until_it_ends() {
    let sources = sources();
    let openers = callers(&sources, "start_login_observed");
    assert!(!openers.is_empty(), "no function starts a sign-in (accounts::start_login_observed). {}", update(MARK));
    for (file, opener, body) in &openers {
        let fail = |what: &str| format!("{opener} in {file}: {what}. {}\n{body}", update(MARK));
        let marks = places(body, ".sign_ins.open()");
        assert_eq!(marks.len(), 1, "{}", fail("the sign-in takes one mark, `.sign_ins.open()`"));
        let mark = marks[0];
        for first in ["start_login_observed(", "signin_session(", "signin_options("] {
            assert!(find(body, first).map_or(true, |at| mark < at), "{}", fail(&format!("the mark is taken before `{first}`")));
        }
        let word = |c: char| c.is_alphanumeric() || c == '_';
        let (at, bound) = places(&body[..mark], "let").into_iter()
            .filter(|&at| !body[..at].chars().next_back().is_some_and(word) && body[at + 3..].starts_with([' ', '(']))
            .last()
            .and_then(|at| statement(&body[at..], "let").filter(|s| at + s.len() > mark).map(|s| (at, s)))
            .unwrap_or_else(|| panic!("{}", fail("the mark is bound with `let`")));
        let name = bound[3..].split('=').next().unwrap().trim().trim_start_matches("mut ");
        assert!(name != "_" && name.chars().all(word), "{}", fail(&format!("the mark is bound to a name, not `{name}`: `let _ =` lets go of it at once")));
        // Taken as it is, never under a condition or in a closure, and in the function's own body,
        // never in a block that ends before the sign-in does.
        let alone = format!("let {name} = app.state::<AppState>().sign_ins.open();");
        let with = format!("let {name} = ({name}, app.state::<AppState>().sign_ins.open());");
        assert!(bound == tokens(&alone) || bound == tokens(&with), "{}", fail(&format!("the mark is taken whatever happens, as `{alone}` or `{with}`")));
        let depth = body[..at].chars().fold(0i32, |depth, c| depth + match c { '{' => 1, '}' => -1, _ => 0 });
        assert_eq!(depth, 1, "{}", fail("the mark is taken in the function's own body, not in a block inside it"));
        let wait = [".wait(", ".wait_isolated("].iter().filter_map(|w| find(body, w)).min()
            .unwrap_or_else(|| panic!("{}", fail("the sign-in is waited for")));
        assert!(find(body, "start_login_observed(").is_some_and(|start| start < wait), "{}", fail("the sign-in is waited for once it starts"));
        for dropped in places(body, &format!("drop({name})")) {
            assert!(dropped > wait, "{}", fail(&format!("`drop({name})` comes only once the sign-in has been waited for")));
        }
        for kept in ["forget(", "::leak(", "ManuallyDrop"] {
            assert!(!has(body, kept), "{}", fail(&format!("nothing keeps the mark from being dropped (`{kept}`)")));
        }
    }
}

/// The helper asks everything that shows a sign-in, and each alone is enough.
#[test]
fn the_helper_asks_everything_that_shows_a_sign_in() {
    let app = tokens(include_str!("../app.rs"));
    let helper = function(&app, "app.rs", "pub(crate) fn sign_in_open(&self) -> bool {", HELPER);
    let asks = "pub(crate) fn sign_in_open(&self) -> bool { any_sign_in_open(signin::any_active(), &self.account_operations, &self.sign_ins) }";
    assert_eq!(helper, tokens(asks), "AppState::sign_in_open asks whether a fresh profile is in use, and its account operations and sign-ins. {}", update(HELPER));
    let (operations, sign_ins) = (monitoring::Operations::default(), monitoring::SignIns::default());
    assert!(!any_sign_in_open(false, &operations, &sign_ins), "nothing is open");
    assert!(any_sign_in_open(true, &operations, &sign_ins), "a fresh profile in use");
    let held = operations.begin_key(monitoring::SIGN_IN).unwrap();
    assert!(any_sign_in_open(false, &operations, &sign_ins), "a monitoring sign-in");
    drop(held);
    let _reserved = operations.begin(Provider::Codex, "add-account").unwrap();
    assert!(!any_sign_in_open(false, &operations, &sign_ins), "an account's reservation is no sign-in by itself");
    let open = sign_ins.open();
    assert!(any_sign_in_open(false, &operations, &sign_ins), "a sign-in's mark");
    drop(open);
    assert!(!any_sign_in_open(false, &operations, &sign_ins));
}
