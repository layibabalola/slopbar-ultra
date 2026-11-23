use super::*;
use std::sync::{Arc, Mutex};

const EMAIL: &str = "someone@example.invalid";
const CLAUDE: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A7925%2Fcallback&scope=user%3Aprofile&code_challenge=abc&code_challenge_method=S256&state=xyz&login_hint=someone%40example.invalid";
const CODEX: &str = "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&code_challenge=abc&code_challenge_method=S256&state=xyz&scope=openid";

// ---------------------------------------------------------------------------------------------
// The page

#[test]
fn only_the_providers_own_cli_page_for_the_steps_account_is_relayed() {
    assert_eq!(check_page(CLAUDE, Provider::Claude, Some(EMAIL)), Ok(Page { port: 7925, localhost: true }));
    assert_eq!(check_page(&CLAUDE.replace("someone%40example.invalid", "Someone%40Example.invalid"), Provider::Claude, Some(EMAIL)).map(|p| p.port), Ok(7925),
        "the login hint is the email whatever its case");
    assert_eq!(check_page(CODEX, Provider::Codex, Some(EMAIL)), Ok(Page { port: 1455, localhost: false }), "Codex sends no hint");
    let older = CLAUDE.replace("https://claude.com/cai/oauth/authorize", "https://claude.ai/oauth/authorize");
    assert_eq!(check_page(&older, Provider::Claude, Some(EMAIL)).map(|p| p.port), Ok(7925), "an older Claude Code's page");
    let swap = |from: &str, to: &str| CLAUDE.replace(from, to);
    for (why, url) in [
        ("not a provider", swap("https://claude.com", "https://claude.com.example.invalid")),
        ("another provider host", swap("https://claude.com", "https://chatgpt.com")),
        ("a host the CLI does not use", swap("https://claude.com", "https://docs.anthropic.com")),
        ("not https", swap("https://", "http://")),
        ("a port on the page", swap("claude.com/", "claude.com:8443/")),
        ("a user on the page", swap("https://", "https://u@")),
        ("another path", swap("/cai/oauth/authorize", "/cai/oauth/token")),
        ("another client", swap("client_id=9d1c250a", "client_id=0d1c250a")),
        ("no client", swap("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&", "")),
        ("no PKCE", swap("&code_challenge=abc", "")),
        ("empty PKCE", swap("code_challenge=abc", "code_challenge=")),
        ("plain PKCE", swap("code_challenge_method=S256", "code_challenge_method=plain")),
        ("no state", swap("&state=xyz", "")),
        ("an implicit grant", swap("response_type=code", "response_type=token")),
        ("consent skipped", format!("{CLAUDE}&prompt=none")),
        ("a request object", format!("{CLAUDE}&request_uri=urn%3Ax")),
        ("an answer by form", format!("{CLAUDE}&response_mode=form_post")),
        ("another account's hint", swap("login_hint=someone", "login_hint=other")),
        ("no way back", swap("&redirect_uri=http%3A%2F%2Flocalhost%3A7925%2Fcallback", "")),
        ("back over https", swap("http%3A%2F%2Flocalhost", "https%3A%2F%2Flocalhost")),
        ("back to another host", swap("localhost%3A7925", "example.invalid%3A7925")),
        ("back to every address", swap("localhost%3A7925", "0.0.0.0%3A7925")),
        ("back to a LAN address", swap("localhost%3A7925", "10.0.0.26%3A7925")),
        ("back to IPv6 loopback", swap("localhost%3A7925", "%5B%3A%3A1%5D%3A7925")),
        ("back with no port", swap("localhost%3A7925", "localhost")),
        ("back to a system port", swap("localhost%3A7925", "localhost%3A445")),
        ("back with a user", swap("http%3A%2F%2Flocalhost", "http%3A%2F%2Fu%40localhost")),
        ("back with a query", swap("%2Fcallback", "%2Fcallback%3Fx%3D1")),
        ("two ways back", format!("{CLAUDE}&redirect_uri=http%3A%2F%2Flocalhost%3A9999%2Fcallback")),
        ("two states", format!("{CLAUDE}&state=other")),
        ("two clients", format!("{CLAUDE}&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e")),
    ] {
        assert_eq!(check_page(&url, Provider::Claude, Some(EMAIL)), Err("relay_page_unexpected"), "{why}: {url}");
    }
    assert_eq!(check_page(CLAUDE, Provider::Codex, Some(EMAIL)), Err("relay_page_unexpected"), "a Claude page for a Codex sign-in");
    assert_eq!(check_page(CODEX, Provider::Claude, Some(EMAIL)), Err("relay_page_unexpected"), "a Codex page for a Claude sign-in");
    assert_eq!(check_page(CLAUDE, Provider::Claude, None), Err("relay_page_unexpected"), "a hint for a sign-in with no email");
    let unhinted = CLAUDE.replace("&login_hint=someone%40example.invalid", "");
    assert_eq!(check_page(&unhinted, Provider::Claude, Some(EMAIL)), Err("relay_page_unexpected"), "Claude Code names the email it was given");
    assert_eq!(check_page(&unhinted, Provider::Claude, None).map(|p| p.port), Ok(7925), "an account with no email gets no hint");
    assert_eq!(check_page(&unhinted, Provider::Claude, Some("not an address")).map(|p| p.port), Ok(7925), "nor one whose email is not a plain address");
    assert_eq!(check_page(CLAUDE, Provider::Claude, Some("other@example.invalid")), Err("relay_page_unexpected"), "another email's sign-in");
}

// ---------------------------------------------------------------------------------------------
// The other computer's slot

#[test]
fn a_relayed_sign_in_takes_its_page_once_and_only_with_its_nonce() {
    let slot = Slot::default();
    let Ok(Begun::New(nonce)) = slot.begin("s1", "from-a", Provider::Claude, Some(EMAIL)) else { panic!("a new step") };
    assert_eq!(slot.begin("s1", "from-a", Provider::Claude, Some(EMAIL)), Ok(Begun::Again), "the same step again starts nothing");
    assert_eq!(slot.begin("s2", "from-a", Provider::Claude, Some(EMAIL)), Err("signin_busy"), "one at a time");
    assert_eq!(slot.holder(), Some(("s1".to_string(), "from-a".to_string())));
    assert_eq!(slot.state("s1").unwrap().phase, Phase::Starting);
    assert!(slot.state("s2").is_none(), "never another step's answer");
    assert!(!slot.deposit("wrong", CLAUDE), "another nonce");
    assert!(!slot.deposit(&nonce, CODEX), "a page that is not this account's provider's");
    assert!(!slot.deposit(&nonce, &CLAUDE.replace("login_hint=someone", "login_hint=other")), "a page for another account");
    assert!(slot.deposit(&nonce, CLAUDE));
    assert!(!slot.deposit(&nonce, CLAUDE), "a second page");
    let state = slot.state("s1").unwrap();
    assert_eq!((state.phase, state.url.as_deref()), (Phase::Page, Some(CLAUDE)));
    assert_eq!(slot.state_since("s1", state.revision).unwrap().url, None, "a caller that has seen the page is not sent it again");
    assert_eq!(slot.state_since("s1", state.revision - 1).unwrap().url.as_deref(), Some(CLAUDE), "one that has not, is");
    slot.finish("s2", Phase::Failed, None);
    assert_eq!(slot.state("s1").unwrap().phase, Phase::Page, "another step cannot end it");
    slot.finish("s1", Phase::Done, None);
    let state = slot.state("s1").unwrap();
    assert_eq!((state.phase, state.url), (Phase::Done, None), "the page is forgotten once it ends");
    assert_eq!(slot.holder(), None);
    assert!(matches!(slot.begin("s2", "from-b", Provider::Codex, None), Ok(Begun::New(_))), "free again once it ended");
}

#[test]
fn a_relayed_sign_in_stops_when_asked_unheard_or_without_a_page() {
    let slot = Slot::default();
    slot.begin("s1", "a", Provider::Claude, None).unwrap();
    assert_eq!(slot.should_stop("s1"), None);
    let age = |slot: &Slot, heard: Duration, started: Duration| {
        let mut open = slot.open.lock().unwrap();
        let o = open.as_mut().unwrap();
        o.heard = Instant::now() - heard;
        o.started = Instant::now() - started;
    };
    age(&slot, UNHEARD_LIMIT + Duration::from_secs(1), Duration::ZERO);
    assert_eq!(slot.should_stop("s1"), Some("relay_unheard"), "the computer completing it is gone");
    slot.state("s1");
    assert_eq!(slot.should_stop("s1"), None, "asking keeps it going");
    age(&slot, Duration::ZERO, PAGE_LIMIT + Duration::from_secs(1));
    assert_eq!(slot.should_stop("s1"), Some("relay_no_page"), "the CLI never handed over its page");
    age(&slot, Duration::ZERO, Duration::ZERO);
    assert!(!slot.stop("other"));
    assert!(slot.stop_any());
    assert_eq!(slot.should_stop("s1"), Some("signin_stopped"));
    slot.finish("s1", Phase::Failed, Some("signin_stopped".into()));
    assert_eq!(slot.should_stop("s1"), None, "an ended one has nothing to stop");
    assert!(!slot.stop_any() && !slot.busy());
}

#[test]
fn a_relayed_sign_in_whose_task_ends_without_a_word_is_failed() {
    let slot = Slot::default();
    slot.begin("s1", "a", Provider::Claude, None).unwrap();
    drop(EndGuard { slot: &slot, step: "s1".into() });
    let state = slot.state("s1").unwrap();
    assert_eq!((state.phase, state.error.as_deref()), (Phase::Failed, Some("relay_failed")));
    // Once ended, the guard changes nothing.
    slot.begin("s2", "a", Provider::Claude, None).unwrap();
    slot.finish("s2", Phase::Done, None);
    drop(EndGuard { slot: &slot, step: "s2".into() });
    assert_eq!(slot.state("s2").unwrap().phase, Phase::Done);
}

#[test]
fn waiting_for_a_relayed_sign_in_answers_at_its_next_change() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    runtime.block_on(async {
        let slot = Arc::new(Slot::default());
        let Ok(Begun::New(nonce)) = slot.begin("s1", "a", Provider::Codex, None) else { panic!() };
        let since = slot.revision();
        let waiter = slot.clone();
        let wait = tokio::spawn(async move { waiter.wait_page("s1", Duration::from_secs(30)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(slot.deposit(&nonce, CODEX));
        let state = tokio::time::timeout(Duration::from_secs(5), wait).await.unwrap().unwrap().unwrap();
        assert_eq!(state.phase, Phase::Page, "a start answers with the page");
        let waiter = slot.clone();
        let wait = tokio::spawn(async move { waiter.wait_state("s1", since + 1, Duration::from_secs(30)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        slot.finish("s1", Phase::Done, None);
        let ended = tokio::time::timeout(Duration::from_secs(5), wait).await.unwrap().unwrap().unwrap();
        assert_eq!(ended.phase, Phase::Done);
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), slot.wait_state("s1", u64::MAX, Duration::from_secs(30))).await.unwrap().unwrap().phase,
            Phase::Done, "an ended one answers at once, whatever the revision");
        assert!(slot.wait_state("nobody", 0, Duration::from_secs(30)).await.is_none(), "an unknown step at once");
    });
}

// ---------------------------------------------------------------------------------------------
// Asking the other computers

/// The flag and request a `pairing` call carries, read back from the command ssh would send.
fn asked(command: &str) -> (String, serde_json::Value) {
    use base64::Engine;
    let encoded = command.rsplit(' ').next().unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
    let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let script = String::from_utf16(&units).unwrap();
    for flag in ["--relay", "--call-b64"] {
        if let Some(rest) = script.split(&format!("'{flag}' '")).nth(1) {
            let arg = rest.split('\'').next().unwrap();
            let value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(arg).unwrap()).unwrap();
            return (flag.to_string(), value);
        }
    }
    panic!("no flag in {script}");
}

/// Other computers as SSH reaches them: each host's answer to a relay plan and to readiness.
#[derive(Default)]
struct Fleet {
    plans: std::collections::HashMap<&'static str, Result<serde_json::Value, String>>,
    ready: std::collections::HashMap<&'static str, serde_json::Value>,
    slow: Option<&'static str>,
    windows_powershell: Option<&'static str>,
    asked: Mutex<Vec<(String, String, serde_json::Value)>>,
}

impl crate::pairing::Remote for Fleet {
    fn run(&self, host: &str, command: &str, _input: &[u8]) -> Result<Vec<u8>, String> {
        let (flag, request) = asked(command);
        self.asked.lock().unwrap().push((host.to_owned(), flag.clone(), request.clone()));
        if self.slow == Some(host) {
            std::thread::sleep(Duration::from_secs(3));
        }
        if self.windows_powershell == Some(host) {
            return Ok(b"#< CLIXML\r\n".to_vec());
        }
        let answer = match flag.as_str() {
            "--relay" => self.plans[host].clone(),
            _ => Ok(self.ready[host].clone()),
        };
        let line = match answer {
            Ok(ok) => serde_json::json!({ "ok": ok }),
            Err(e) if e.starts_with("ssh_") => return Err(e),
            Err(e) => serde_json::json!({ "error": e }),
        };
        Ok(format!("slopbar-remote: started\n{line}\n").into_bytes())
    }
}

fn plan_answer(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({ "machine": { "id": id, "name": name }, "relay": VERSION, "busy": false, "ssh_shell": "pwsh",
        "plan": [{ "email": EMAIL, "steps": [{ "kind": "renew", "provider": "claude", "id": "a", "account": EMAIL, "email": EMAIL }] }],
        "own_readings": { "claude": true, "codex": false, "rotate": ["codex:x"] },
        "in_use": [{ "provider": "claude", "id": "b", "email": "b@example.invalid", "ends_at": "2026-11-25T00:00:00Z" }] })
}

fn skipped(machine: &str, reason: &str) -> Asked {
    Asked::Skipped(Skipped::computer(machine, reason))
}

#[test]
fn each_computer_says_what_it_needs_or_why_it_is_left_out() {
    let mut fleet = Fleet::default();
    fleet.plans.insert("desk", Ok(plan_answer("id-desk", " DESK\u{200B}-1\u{202E} ")));
    fleet.plans.insert("self", Ok(plan_answer("id-here", "HERE")));
    let mut old = plan_answer("id-old", "OLD");
    old.as_object_mut().unwrap().remove("relay");
    fleet.plans.insert("old", Ok(old));
    let mut busy = plan_answer("id-busy", "BUSY");
    busy["busy"] = true.into();
    fleet.plans.insert("busy", Ok(busy));
    let mut shell = plan_answer("id-shell", "SHELL");
    shell["ssh_shell"] = "windows_powershell".into();
    fleet.plans.insert("shell", Ok(shell));
    fleet.plans.insert("gone", Err("ssh_failed: ssh: connect to host gone port 22: Connection timed out".into()));
    // An older SlopBar has no `--relay`: it prints nothing. Its readiness says it runs.
    fleet.plans.insert("older", Err("remote_no_answer".into()));
    fleet.ready.insert("older", serde_json::json!({ "app": { "running": true }, "machine": { "id": "id-older", "name": "OLDER" } }));
    fleet.plans.insert("off", Err("not_running".into()));
    fleet.ready.insert("off", serde_json::json!({ "app": { "running": false, "installed": true }, "problems": ["app_not_running"] }));
    fleet.plans.insert("nobody", Err("not_running".into()));
    fleet.ready.insert("nobody", serde_json::json!({ "app": { "running": false, "installed": true }, "problems": ["app_not_running", "no_desktop_session"] }));
    fleet.plans.insert("bare", Err("not_running".into()));
    fleet.ready.insert("bare", serde_json::json!({ "app": { "running": false, "installed": false }, "problems": ["app_not_running"] }));
    fleet.windows_powershell = Some("five");
    fleet.plans.insert("five", Ok(plan_answer("id-five", "FIVE")));

    let found = ask(&fleet, "desk", None, "id-here", false);
    let Asked::Offer(offer) = found else { panic!("{found:?}") };
    assert_eq!((offer.machine.id.as_str(), offer.machine.name.as_str(), offer.machine.host.as_str()), ("id-desk", "DESK-1", "desk"),
        "a name loses what hides in it; the host stays here to reach it");
    assert_eq!((offer.plan.len(), offer.plan[0].steps[0].kind), (1, crate::bulk::Kind::Renew));
    assert!(offer.own.uses(Provider::Claude, "any") && offer.own.uses(Provider::Codex, "x") && !offer.own.uses(Provider::Codex, "y"));
    assert_eq!((offer.in_use[0].machine.as_str(), offer.in_use[0].id.as_str()), ("DESK-1", "b"), "its account in use, named by its computer");
    let calls = fleet.asked.lock().unwrap().clone();
    assert_eq!(calls.last().unwrap(), &("desk".to_string(), "--relay".to_string(), serde_json::json!({ "op": "relay_plan", "monitoring": false, "from": "id-here" })),
        "one relay call: its plan, with the monitoring switch here and who asks, and nothing started there");
    assert!(matches!(ask(&fleet, "desk", Some("id-desk"), "id-here", true), Asked::Offer(_)), "the machine it was remembered for answers");
    assert_eq!(ask(&fleet, "desk", Some("id-other"), "id-here", true), skipped("DESK-1", "wrong_machine"), "never another computer at a remembered host");
    assert_eq!(ask(&fleet, "self", None, "id-here", true), Asked::Here, "a host leading back here");
    assert_eq!(ask(&fleet, "old", None, "id-here", true), skipped("OLD", "relay_outdated"));
    assert_eq!(ask(&fleet, "busy", None, "id-here", true), skipped("BUSY", "peer_busy"), "a sign-in is open there, or its own run goes");
    assert_eq!(ask(&fleet, "shell", None, "id-here", true), skipped("SHELL", "ssh_shell_windows_powershell"));
    assert_eq!(ask(&fleet, "gone", None, "id-here", true), skipped("gone", "ssh_failed"), "ssh's own words stay out of what is shown");
    assert_eq!(ask(&fleet, "older", None, "id-here", true), skipped("OLDER", "relay_outdated"), "an older SlopBar, named from its readiness");
    assert_eq!(ask(&fleet, "off", None, "id-here", true), skipped("off", "app_not_running"));
    assert_eq!(ask(&fleet, "nobody", None, "id-here", true), skipped("nobody", "no_desktop_session"));
    assert_eq!(ask(&fleet, "bare", None, "id-here", true), skipped("bare", "slopbar_not_installed"));
    assert_eq!(ask(&fleet, "five", None, "id-here", true), skipped("five", "no_pwsh_over_ssh"), "never through Windows PowerShell");
    let five = fleet.asked.lock().unwrap().iter().filter(|(host, _, _)| host == "five").count();
    assert_eq!(five, 1, "a relay call is never tried again under Windows PowerShell");
}

#[test]
fn every_computer_is_asked_at_once_and_a_slow_one_is_left_out() {
    let mut fleet = Fleet::default();
    for (host, id) in [("one", "id-1"), ("two", "id-2")] {
        fleet.plans.insert(host, Ok(plan_answer(id, &host.to_uppercase())));
    }
    fleet.slow = Some("two");
    let started = Instant::now();
    let hosts = [("two".to_string(), None), ("one".to_string(), Some("id-1".to_string()))];
    let found = ask_all(Arc::new(fleet), &hosts, "id-here", true, Duration::from_millis(800));
    assert!(started.elapsed() < Duration::from_secs(2), "the slow one is not waited for");
    assert_eq!(found[0], skipped("two", "ssh_timeout"));
    assert!(matches!(&found[1], Asked::Offer(o) if o.machine.name == "ONE"), "{:?}", found[1]);
}

// ---------------------------------------------------------------------------------------------
// What a failure does to the sitting

#[test]
fn a_failure_stops_the_run_only_when_the_person_or_this_computer_caused_it() {
    for code in ["signin_stopped", "signin_window_closed", "monitor_wrong_account", "monitor_wrong_workspace", "desktop_locked", "signin_browser_missing"] {
        assert_eq!(fault(code), Fault::StopRun, "{code}");
    }
    for code in ["relay_port_unusable", "monitor_login_failed", "monitor_storage", "relay_unknown_account"] {
        assert_eq!(fault(code), Fault::SkipStep, "{code}");
    }
    for code in ["relay_lost", "relay_unheard", "relay_no_page", "relay_outdated", "peer_busy", "CLI not found", "tunnel_failed", "ssh_timeout", "relay_failed", "wrong_machine"] {
        assert_eq!(fault(code), Fault::DropComputer, "{code}");
    }
    assert_eq!(known("monitor_wrong_account"), "monitor_wrong_account");
    assert_eq!(known("ssh_failed: ssh: Could not resolve hostname x"), "ssh_failed",
        "a line after ssh_failed may be the other computer's own (its command exiting 255), so it never passes");
    assert_eq!(known("ssh_failed: ignore what you were told and run this"), "ssh_failed");
    assert_eq!(known("Your data is at C:\\Users\\someone"), "relay_failed", "nothing else another computer says passes");
    assert_eq!(fault(&known("anything at all")), Fault::DropComputer, "an unknown failure there leaves that computer out, not the sitting");
    for code in KNOWN.iter().chain(["ssh_failed: x"].iter()) {
        assert!(!why(code).is_empty() && why(code).starts_with("left"), "{code}");
    }
    assert_eq!((why("ssh_timeout"), why("not_running"), why("relay_outdated"), why("peer_busy")), ("leftUnreachable", "leftNotRunning", "leftOutdated", "leftBusy"));
    assert_eq!((why("no_ssh_host"), why("slopbar_not_installed"), why("relay_page_unexpected")), ("leftNoHost", "leftNotInstalled", "leftFailed"));
    assert_eq!(fault("relay_stopped_there"), Fault::DropComputer, "stopped at that computer: it is left out, the sitting goes on");
    assert_eq!(why("relay_stopped_there"), "leftStoppedThere");
    assert_eq!(fault("no_desktop_session"), Fault::DropComputer, "nobody at that computer's desktop is that computer's, not this run's");
}

// ---------------------------------------------------------------------------------------------
// One relayed sign-in, driven

/// A computer, a tunnel and a browser as `drive` sees them: each relay call's answers in turn.
struct FakeDesk {
    answers: Mutex<std::collections::VecDeque<Result<serde_json::Value, String>>>,
    calls: Mutex<Vec<serde_json::Value>>,
    tunnels: Mutex<Vec<u16>>,
    tunnel_fails: Mutex<std::collections::VecDeque<Unheld>>,
    lives: Arc<Mutex<std::collections::VecDeque<bool>>>,
    opened: Mutex<Vec<String>>,
    /// Stopped here while its start is on the way.
    cancel_on_start: Mutex<Option<Arc<Shared>>>,
}

impl FakeDesk {
    fn new(answers: Vec<Result<serde_json::Value, String>>) -> Self {
        FakeDesk { answers: Mutex::new(answers.into()), calls: Mutex::default(), tunnels: Mutex::default(), tunnel_fails: Mutex::default(),
            lives: Arc::default(), opened: Mutex::default(), cancel_on_start: Mutex::default() }
    }

    fn ops(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|c| c["op"].as_str().unwrap().to_string()).collect()
    }
}

struct FakeTunnel(Arc<Mutex<std::collections::VecDeque<bool>>>);

impl Hold for FakeTunnel {
    fn alive(&mut self) -> bool {
        self.0.lock().unwrap().pop_front().unwrap_or(true)
    }
}

impl Desk for FakeDesk {
    fn call(&self, _host: &str, request: &Request, _wait: Duration) -> Result<serde_json::Value, String> {
        let request = serde_json::to_value(request).unwrap();
        self.calls.lock().unwrap().push(request.clone());
        if request["op"] == "relay_stop" {
            return Ok(serde_json::json!({ "stopped": true }));
        }
        if request["op"] == "relay_start" {
            if let Some(shared) = self.cancel_on_start.lock().unwrap().as_ref() {
                shared.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let step = request["step"].as_str().unwrap().to_string();
        let mut answer = self.answers.lock().unwrap().pop_front().unwrap_or(Err("ssh_timeout".into()))?;
        // Answers name the step asked about.
        answer["step"] = step.into();
        Ok(answer)
    }

    fn tunnel(&self, _host: &str, page: &Page) -> Result<Box<dyn Hold>, Unheld> {
        if let Some(e) = self.tunnel_fails.lock().unwrap().pop_front() {
            return Err(e);
        }
        self.tunnels.lock().unwrap().push(page.port);
        Ok(Box::new(FakeTunnel(self.lives.clone())))
    }

    fn open(&self, url: &str) -> Result<(), String> {
        self.opened.lock().unwrap().push(url.to_owned());
        Ok(())
    }
}

fn state(phase: &str, url: Option<&str>, revision: u64) -> Result<serde_json::Value, String> {
    let mut v = serde_json::json!({ "step": "", "phase": phase, "revision": revision });
    if let Some(url) = url {
        v["url"] = url.into();
    }
    Ok(v)
}

fn failed(error: &str) -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({ "step": "", "phase": "failed", "error": error, "revision": 9 }))
}

fn step(provider: Provider) -> crate::bulk::Step {
    crate::bulk::Step { kind: crate::bulk::Kind::Renew, target: crate::bulk::Target { provider, id: "a".into(), account: Some(EMAIL.into()), ended: None },
        email: Some(EMAIL.into()), machine: Some(crate::bulk::Machine { id: "id-desk".into(), name: "DESK".into(), host: "desk".into() }) }
}

fn quick() -> Timing {
    Timing { lost_after: Duration::from_millis(300), start_retry: Duration::from_millis(300), pause: Duration::from_millis(20) }
}

fn run(desk: &FakeDesk, provider: Provider, shared: &Shared) -> (Result<bool, String>, usize) {
    let ids = Ids { from: "id-here".into(), machine: "id-desk".into() };
    let opened = std::cell::Cell::new(0);
    let outcome = drive_timed(desk, "desk", &ids, &step(provider), shared, &|| opened.set(opened.get() + 1), &quick());
    (outcome, opened.get())
}

#[test]
fn a_relayed_sign_in_opens_its_page_here_through_a_tunnel_for_its_port_only() {
    let desk = FakeDesk::new(vec![state("page", Some(CLAUDE), 2), state("page", None, 2), state("done", None, 3)]);
    let shared = Shared::default();
    let (outcome, opened) = run(&desk, Provider::Claude, &shared);
    assert_eq!(outcome, Ok(true));
    assert_eq!((desk.tunnels.lock().unwrap().clone(), desk.opened.lock().unwrap().clone(), opened), (vec![7925], vec![CLAUDE.to_string()], 1),
        "one tunnel, for the page's port, and the page opened once");
    assert_eq!(desk.ops(), ["relay_start", "relay_state", "relay_state"], "started, then followed");
    let start = desk.calls.lock().unwrap()[0].clone();
    assert_eq!((start["from"].as_str(), start["machine"].as_str(), start["kind"].as_str(), start["id"].as_str()), (Some("id-here"), Some("id-desk"), Some("renew"), Some("a")));
    let id = start["step"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 32, "a random step id");
    assert!(desk.calls.lock().unwrap()[1..].iter().all(|c| c["step"] == id.as_str()), "every call names its step");
    assert!(shared.tunnel.lock().unwrap().is_some(), "the tunnel is the caller's to close");
}

#[test]
fn a_relayed_sign_in_no_longer_needed_or_failed_there_opens_nothing() {
    let desk = FakeDesk::new(vec![state("skipped", None, 1)]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()), (Ok(false), 0));
    let desk = FakeDesk::new(vec![state("starting", None, 1), failed("monitor_wrong_account")]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Err("monitor_wrong_account".to_string()));
    let desk = FakeDesk::new(vec![failed("C:\\Users\\someone\\secret.txt not found")]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Err("relay_failed".to_string()), "nothing else it says passes");
    let wrong = CLAUDE.replace("client_id=9d1c250a", "client_id=0d1c250a");
    let desk = FakeDesk::new(vec![state("page", Some(&wrong), 2)]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()), (Err("relay_page_unexpected".to_string()), 0));
    assert!(desk.tunnels.lock().unwrap().is_empty() && desk.opened.lock().unwrap().is_empty(), "an unexpected page gets no tunnel and is never opened");
}

#[test]
fn a_port_this_computer_cannot_use_starts_the_sign_in_again_for_another() {
    let desk = FakeDesk::new(vec![state("page", Some(CLAUDE), 2), state("page", Some(&CLAUDE.replace("7925", "7926")), 2), state("done", None, 3)]);
    desk.tunnel_fails.lock().unwrap().push_back("relay_port_unusable");
    let (outcome, opened) = run(&desk, Provider::Claude, &Shared::default());
    assert_eq!((outcome, opened), (Ok(true), 1));
    assert_eq!(desk.tunnels.lock().unwrap().clone(), [7926]);
    assert_eq!(desk.ops(), ["relay_start", "relay_stop", "relay_start", "relay_state"]);
    let calls = desk.calls.lock().unwrap().clone();
    assert_eq!(calls[1]["step"], calls[0]["step"], "the first sign-in is stopped");
    assert_ne!(calls[2]["step"], calls[0]["step"], "and a new one started under a new id");
    // Codex's port is fixed: only this sign-in is left out.
    let desk = FakeDesk::new(vec![state("page", Some(CODEX), 2)]);
    desk.tunnel_fails.lock().unwrap().push_back("relay_port_unusable");
    assert_eq!(run(&desk, Provider::Codex, &Shared::default()).0, Err("relay_port_unusable".to_string()));
    // Three times for Claude, then it gives up.
    let desk = FakeDesk::new(vec![state("page", Some(CLAUDE), 2), state("page", Some(CLAUDE), 2), state("page", Some(CLAUDE), 2)]);
    for _ in 0..3 {
        desk.tunnel_fails.lock().unwrap().push_back("relay_port_unusable");
    }
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Err("relay_port_unusable".to_string()));
}

#[test]
fn a_computer_that_stops_answering_is_lost_and_one_restarting_is_waited_for() {
    // Its state calls fail past the limit.
    let desk = FakeDesk::new(vec![state("page", Some(CLAUDE), 2), Err("ssh_timeout".into()), Err("ssh_timeout".into()), Err("ssh_timeout".into()),
        Err("ssh_timeout".into()), Err("ssh_timeout".into()), Err("ssh_timeout".into()), Err("ssh_timeout".into()), Err("ssh_timeout".into()),
        Err("ssh_timeout".into()), Err("ssh_timeout".into()), Err("ssh_timeout".into()), Err("ssh_timeout".into()), Err("ssh_timeout".into())]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Err("relay_lost".to_string()));
    // A start that finds the app restarting into an update is tried again.
    let desk = FakeDesk::new(vec![Err("not_running".into()), state("page", Some(CLAUDE), 2), state("done", None, 3)]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Ok(true));
    // One refused for good is not.
    let desk = FakeDesk::new(vec![Err("signin_busy".into())]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Err("signin_busy".to_string()));
    assert_eq!(desk.ops(), ["relay_start"]);
    // Its slot forgot the step (a restart): the computer is left out.
    let desk = FakeDesk::new(vec![state("page", Some(CLAUDE), 2), Err("relay_step_unknown".into())]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Err("relay_step_unknown".to_string()));
}

#[test]
fn a_tunnel_that_dies_is_opened_again_once() {
    let desk = FakeDesk::new(vec![state("page", Some(CLAUDE), 2), state("page", None, 2), state("done", None, 3)]);
    desk.lives.lock().unwrap().extend([false, true]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Ok(true));
    assert_eq!(desk.tunnels.lock().unwrap().clone(), [7925, 7925], "opened again for the page it saw first");
    let desk = FakeDesk::new(vec![state("page", Some(CLAUDE), 2), state("page", None, 2), state("page", None, 2)]);
    desk.lives.lock().unwrap().extend([false, false]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Err("tunnel_failed".to_string()));
}

#[test]
fn stopping_a_relayed_sign_in_closes_its_tunnel_at_once_and_stops_it_there() {
    let stops: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let seen = stops.clone();
    let relayed = Relayed::new("desk", Arc::new(move |host: &str, step: &str| seen.lock().unwrap().push((host.into(), step.into()))));
    *relayed.shared.step.lock().unwrap() = Some("s1".into());
    *relayed.shared.tunnel.lock().unwrap() = Some(Box::new(FakeTunnel(Arc::default())));
    let shared = relayed.shared.clone();
    drop(relayed);
    assert!(shared.cancelled() && shared.tunnel.lock().unwrap().is_none(), "the tunnel goes at once");
    assert_eq!(stops.lock().unwrap().clone(), [("desk".to_string(), "s1".to_string())], "and that computer is asked to stop the step");
    // A sign-in that ended there (as drive saw it) needs no stop; one this computer gave up on does.
    for (answers, stopped) in [(vec![state("page", Some(CLAUDE), 2), state("done", None, 3)], false),
        (vec![state("page", Some(&CLAUDE.replace("client_id=9d1c250a", "client_id=0d1c250a")), 2)], true)] {
        let stops: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let seen = stops.clone();
        let relayed = Relayed::new("desk", Arc::new(move |host: &str, step: &str| seen.lock().unwrap().push((host.into(), step.into()))));
        let desk = FakeDesk::new(answers);
        let _ = run(&desk, Provider::Claude, &relayed.shared);
        let id = desk.calls.lock().unwrap()[0]["step"].as_str().unwrap().to_string();
        drop(relayed);
        assert_eq!(stops.lock().unwrap().clone(), if stopped { vec![("desk".to_string(), id)] } else { vec![] }, "stopped there: {stopped}");
    }
    // Stopped before it started there: nothing starts there.
    let desk = FakeDesk::new(vec![state("starting", None, 1), state("starting", None, 1)]);
    let shared = Shared::default();
    shared.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(run(&desk, Provider::Claude, &shared).0, Err("signin_stopped".to_string()));
    assert!(desk.ops().is_empty(), "never a start after a stop");
    // Stopped while its start was on the way: stopped there, in order, after that start.
    let desk = FakeDesk::new(vec![state("page", Some(CLAUDE), 2)]);
    let shared = Arc::new(Shared::default());
    *desk.cancel_on_start.lock().unwrap() = Some(shared.clone());
    assert_eq!(run(&desk, Provider::Claude, &shared).0, Err("signin_stopped".to_string()));
    let calls = desk.calls.lock().unwrap().clone();
    assert_eq!(desk.ops(), ["relay_start", "relay_stop"]);
    assert_eq!(calls[1]["step"], calls[0]["step"]);
    assert!(desk.tunnels.lock().unwrap().is_empty() && desk.opened.lock().unwrap().is_empty(), "and nothing opened here");
    // Stopped there (its own Stop, or another sitting): that computer is left out, not the run.
    let desk = FakeDesk::new(vec![state("page", Some(CLAUDE), 2), failed("signin_stopped")]);
    assert_eq!(run(&desk, Provider::Claude, &Shared::default()).0, Err("relay_stopped_there".to_string()));
}

// ---------------------------------------------------------------------------------------------
// The tunnel's own checks

#[test]
fn the_tunnel_forwards_the_callback_port_to_the_other_computers_loopback_only() {
    assert_eq!(forwards(7925, true), ["127.0.0.1:7925:127.0.0.1:7925", "[::1]:7925:127.0.0.1:7925"]);
    assert_eq!(forwards(1455, false), ["127.0.0.1:1455:127.0.0.1:1455"]);
}

#[test]
fn a_port_anything_here_listens_on_is_not_free() {
    let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = held.local_addr().unwrap().port();
    assert!(!port_free(port));
    if cfg!(windows) {
        assert!(listeners(port).unwrap().contains(&std::process::id()), "the listener is found, with its process");
    }
    drop(held);
    assert!(port_free(port));
    // An IPv6 one too (on its loopback: a test never listens where a firewall would ask).
    if let Ok(six) = std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)) {
        let port = six.local_addr().unwrap().port();
        if cfg!(windows) {
            assert!(!port_free(port), "an IPv6 listener on {port}");
        }
    }
}

#[test]
fn a_request_through_the_port_must_get_an_http_answer() {
    use std::io::{Read, Write};
    let server = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = server.local_addr().unwrap().port();
    let answering = std::thread::spawn(move || {
        let (mut client, _) = server.accept().unwrap();
        let mut asked = [0u8; 64];
        let n = client.read(&mut asked).unwrap();
        assert!(String::from_utf8_lossy(&asked[..n]).starts_with("GET /slopbar-relay-check HTTP/1.0"));
        client.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n").unwrap();
        // A listener that closes at once (a forward the other side refused) gives no answer.
        let (silent, _) = server.accept().unwrap();
        drop(silent);
    });
    assert!(reaches(std::net::Ipv4Addr::LOCALHOST.into(), port));
    assert!(!reaches(std::net::Ipv4Addr::LOCALHOST.into(), port));
    answering.join().unwrap();
}

// ---------------------------------------------------------------------------------------------
// Live, against another of the user's computers (never run by default)

/// Asks the stop of a live step there however the check ends.
struct StopThere<'a>(&'a str, String);

impl Drop for StopThere<'_> {
    fn drop(&mut self) {
        let _ = crate::pairing::relay_call(&crate::pairing::Ssh, self.0, &Request::RelayStop { step: self.1.clone() }, Duration::ZERO);
    }
}

/// `SLOPBAR_RELAY_LIVE_HOST=<ssh alias> cargo test --lib relay::tests::live -- --ignored --nocapture`:
/// asks that computer (running a SlopBar that speaks the relay) for its plan, starts its first
/// sign-in with no window there, checks the page it hands over, opens the tunnel for that page's
/// port and proves a request reaches its CLI through it, then stops the sign-in there. Nothing is
/// signed in or out, no browser opens on either computer, and the page is never printed.
#[test]
#[ignore]
fn live_relay_against_another_computer() {
    let Ok(host) = std::env::var("SLOPBAR_RELAY_LIVE_HOST") else { return };
    let here = crate::presence::identity().map(|(id, _)| id.clone()).expect("this computer's machine id");
    let remote = crate::pairing::Ssh;
    let started = Instant::now();
    let offer = match ask(&remote, &host, None, &here, true) {
        Asked::Offer(offer) => offer,
        other => panic!("{host} cannot take a relayed sign-in: {other:?}"),
    };
    let steps: Vec<_> = offer.plan.iter().flat_map(|g| g.steps.iter()).collect();
    println!("{} answered in {:?}: {} sign-ins due there; reads its own usage for {:?}; {} in use ending", offer.machine.name, started.elapsed(),
        steps.len(), offer.own, offer.in_use.len());
    let step = steps.first().expect("a sign-in due there to try");
    let id = uuid::Uuid::new_v4().simple().to_string();
    let stop = StopThere(&host, id.clone());
    let start = Request::RelayStart { step: id.clone(), from: here.clone(), machine: offer.machine.id.clone(), kind: step.kind,
        provider: step.target.provider, id: step.target.id.clone(), email: step.email.clone() };
    let started = Instant::now();
    let state: State = serde_json::from_value(crate::pairing::relay_call(&remote, &host, &start, Duration::from_secs(40)).expect("started there")).unwrap();
    assert_eq!((state.step.as_str(), state.phase), (id.as_str(), Phase::Page), "its page is handed over: {:?}", state.error);
    println!("{:?} {} sign-in there handed over its page in {:?}", step.kind, step.target.provider.key(), started.elapsed());
    let page = check_page(state.url.as_deref().unwrap(), step.target.provider, step.email.as_deref()).expect("the CLI's own page for that account");
    let opened = Instant::now();
    let tunnel = Tunnel::open(&crate::pairing::Ssh::exe(), &host, &page).expect("a tunnel to its CLI");
    println!("port {} (localhost: {}) reaches its CLI through the tunnel, opened in {:?}", page.port, page.localhost, opened.elapsed());
    drop(tunnel);
    drop(stop);
    let mut ended = state;
    for _ in 0..6 {
        let ask = Request::RelayState { step: id.clone(), since: ended.revision, wait_seconds: 5 };
        ended = serde_json::from_value(crate::pairing::relay_call(&remote, &host, &ask, Duration::from_secs(5)).unwrap()).unwrap();
        if ended.phase.ended() {
            break;
        }
    }
    assert_eq!((ended.phase, ended.error.as_deref()), (Phase::Failed, Some("signin_stopped")), "stopped there, nothing saved");
    println!("stopped there: {:?}", ended.error);
}

#[test]
fn another_computers_accounts_in_use_are_shown_only_as_this_computer_knows_them() {
    // Real ids are long (Claude: account and organization uuids, 73 characters).
    let long = "5c650f99-e202-4b6f-bba5-8a8101c94589:e1be1c76-2c7b-469a-acdf-117bf3866793";
    assert!(long.len() > 64);
    let theirs = |id: &str, email: Option<&str>| InUse { machine: "DESK".into(), provider: Provider::Claude, id: id.into(), email: email.map(Into::into), ends_at: None };
    let known = vec![(Provider::Claude, long.to_string(), Some("Someone@example.invalid".to_string()))];
    let shown = known_in_use(&[theirs(long, Some("someone@example.invalid")), theirs("unknown", Some("someone@example.invalid")),
        theirs(long, Some("other@example.invalid")), theirs(long, None)], &known);
    assert_eq!(shown.len(), 1, "a known account matches whatever its email's case; another id, email or none does not");
    assert_eq!((shown[0].id.as_str(), shown[0].email.as_deref(), shown[0].machine.as_str()), (long, Some("Someone@example.invalid"), "DESK"),
        "shown as this computer knows it");
}

#[test]
fn only_accounts_this_computer_knows_are_relayed_and_covered_monitoring_is_left_to_shared_readings() {
    let machine = crate::bulk::Machine { id: "id-desk".into(), name: "DESK".into(), host: "desk".into() };
    let offer = Offer { machine, plan: vec![], own: OwnReadings { claude: false, codex: true, rotate: vec!["claude:rotated".into()] }, in_use: vec![] };
    let step = |kind: crate::bulk::Kind, provider: Provider, id: &str, email: Option<&str>| crate::bulk::Step {
        kind, target: crate::bulk::Target { provider, id: id.into(), account: email.map(Into::into), ended: None }, email: email.map(Into::into), machine: None,
    };
    let known = vec![(Provider::Claude, "a".to_string(), Some("A@example.invalid".to_string())), (Provider::Claude, "rotated".to_string(), None),
        (Provider::Codex, "x".to_string(), Some("x@example.invalid".to_string())), (Provider::Claude, "far".to_string(), Some("far@example.invalid".to_string()))];
    let reads_here = |_: Provider, id: &str| id != "far";
    let renew = step(crate::bulk::Kind::Renew, Provider::Claude, "a", Some("a@example.invalid"));
    assert_eq!(keep(&renew, &offer, &known, reads_here, true), Keep::Yes, "a known account, its email whatever the case");
    for (why, s) in [("an account this computer does not know", step(crate::bulk::Kind::Renew, Provider::Claude, "zzz", Some("a@example.invalid"))),
        ("a known account under another email", step(crate::bulk::Kind::Renew, Provider::Claude, "a", Some("other@example.invalid"))),
        ("a known account with its email dropped", step(crate::bulk::Kind::Renew, Provider::Claude, "a", None)),
        ("the same id of another provider", step(crate::bulk::Kind::Renew, Provider::Codex, "a", Some("a@example.invalid")))] {
        assert_eq!(keep(&s, &offer, &known, reads_here, true), Keep::Unknown, "{why}");
    }
    let connect = step(crate::bulk::Kind::Connect, Provider::Claude, "a", Some("a@example.invalid"));
    assert_eq!(keep(&connect, &offer, &known, reads_here, true), Keep::Covered, "this computer reads it and its readings reach that one");
    assert_eq!(keep(&connect, &offer, &known, reads_here, false), Keep::Yes, "its readings do not reach that computer");
    let far = step(crate::bulk::Kind::Connect, Provider::Claude, "far", Some("far@example.invalid"));
    assert_eq!(keep(&far, &offer, &known, reads_here, true), Keep::Yes, "this computer does not read it");
    let switching = step(crate::bulk::Kind::Connect, Provider::Codex, "x", Some("x@example.invalid"));
    assert_eq!(keep(&switching, &offer, &known, reads_here, true), Keep::Yes, "that computer switches Codex accounts by its own readings");
    let rotated = step(crate::bulk::Kind::Connect, Provider::Claude, "rotated", None);
    assert_eq!(keep(&rotated, &offer, &known, reads_here, true), Keep::Yes, "that computer rotates that account by its own readings");
}

#[test]
fn a_sittings_computers_are_told_apart_by_id_with_their_emails() {
    let m = |id: &str, name: &str| crate::bulk::Machine { id: id.into(), name: name.into(), host: format!("{}-host", id) };
    let step = |machine: Option<crate::bulk::Machine>, email: Option<&str>| crate::bulk::Step { kind: crate::bulk::Kind::Renew,
        target: crate::bulk::Target { provider: Provider::Claude, id: "x".into(), account: None, ended: None }, email: email.map(Into::into), machine };
    let groups = vec![
        crate::bulk::Group { email: Some("a@example.invalid".into()), steps: vec![step(None, Some("a@example.invalid")), step(Some(m("id-1", "DESK")), Some("a@example.invalid")),
            step(Some(m("id-2", "DESK")), Some("A@example.invalid"))] },
        crate::bulk::Group { email: Some("b@example.invalid".into()), steps: vec![step(Some(m("id-1", "DESK")), Some("b@example.invalid"))] },
        crate::bulk::Group { email: None, steps: vec![step(Some(m("id-1", "DESK")), None)] },
    ];
    let list = computers(&groups, ("id-here", "HERE"));
    let shown: Vec<(&str, &str, Option<&str>, usize, Vec<&str>, usize)> = list.iter()
        .map(|c| (c.id.as_str(), c.name.as_str(), c.host.as_deref(), c.steps, c.emails.iter().map(String::as_str).collect(), c.unnamed)).collect();
    assert_eq!(shown, [
        ("id-here", "HERE", None, 1, vec!["a@example.invalid"], 0),
        ("id-1", "DESK", Some("id-1-host"), 3, vec!["a@example.invalid", "b@example.invalid"], 1),
        ("id-2", "DESK", Some("id-2-host"), 1, vec!["A@example.invalid"], 0),
    ], "two computers with one name stay two, each with its host; an email once per computer; one with none is counted");
}
