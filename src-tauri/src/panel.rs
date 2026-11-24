//! The flyout: opens above the taskbar widget when it is clicked, like the flyouts of the
//! built-in tray items (quick settings, calendar), and closes when it loses focus.
//!
//! A click must open it at once, like the shell's own flyouts. Building a WebView2 takes
//! seconds, so the flyout is built hidden shortly after startup (`prepare`) and is only ever
//! hidden, never destroyed. (WebView2's low memory target while hidden was tried: the flyout then
//! showed blank, so it is not used.) Once it has been on screen, hiding suspends its webview.
//!
//! A closed flyout keeps what it last drew, so every open after the first shows a drawn flyout at
//! once. One that has never been on screen has nothing to show: the first open after a start
//! showed an empty panel, then a grey and an almost black one, and was drawn only 0.3 to 0.5 s
//! after the click (later on a busy machine). So until its first open, the flyout is drawn
//! off-screen once its page has laid out and again whenever its size changes (`warm`), and the
//! page keeps its accounts current meanwhile (main.js), so that open finds it drawn at its size.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, Theme, WindowEvent};

use crate::AppState;

pub const LABEL: &str = "panel";
const WIDTH: f64 = 380.0;
/// Windows 11 flyouts keep 12 px from the taskbar and the screen edge.
const MARGIN: f64 = 12.0;
/// Longest wait for a new flyout's page to lay itself out before it is shown anyway.
const FIRST_PAINT: Duration = Duration::from_millis(1500);
/// How long after startup the hidden flyout is built, so it does not compete with the first
/// refresh and the widget's first paint.
const PREPARE_AFTER: Duration = Duration::from_secs(2);
/// Longest wait for the page to draw while the flyout is off-screen (`warm`).
const WARM_DRAW: Duration = Duration::from_millis(1500);
/// How long a hidden flyout's new size must hold before it is drawn at it: a refresh can lay the
/// page out several times in a row.
const WARM_SETTLE: Duration = Duration::from_millis(300);

/// The page has reported its height since the window was built (see `page_ready`).
static READY: AtomicBool = AtomicBool::new(false);
/// A click arrived while the flyout was still being built or drawn: show it once it is.
static SHOW_WHEN_BUILT: AtomicBool = AtomicBool::new(false);
/// The flyout is being built (by `prepare` or a click), so a second build is not started.
static BUILDING: AtomicBool = AtomicBool::new(false);
/// The flyout's webview has been on screen at least once, so hiding may suspend it.
static SHOWN: AtomicBool = AtomicBool::new(false);
/// The flyout's webview was suspended when it was last hidden.
static SUSPENDED: AtomicBool = AtomicBool::new(false);
/// The flyout is being drawn off-screen (`warm`).
static WARMING: AtomicBool = AtomicBool::new(false);
/// The flyout is open: shown by a click (`show`) and not closed since. Only then does losing focus
/// close it (`on_blur`).
static OPEN: AtomicBool = AtomicBool::new(false);
/// The page has drawn a frame since `warm` asked it to (`drawn`).
static DRAWN: AtomicBool = AtomicBool::new(false);
/// The window size (physical) the hidden flyout was last drawn at, before its first open.
static WARM_SIZE: Mutex<Option<(u32, u32)>> = Mutex::new(None);
/// Counts the hidden flyout's size reports, so only the last of a burst draws it (`keep_drawn`).
static WARM_GEN: AtomicU64 = AtomicU64::new(0);

/// The flyout's page has laid itself out (it reports its height through `set_panel_height`).
pub fn page_ready() {
    READY.store(true, Ordering::Release);
}

/// The flyout's page has drawn a frame since `warm` asked it to (`panel_drawn`).
pub fn drawn() {
    DRAWN.store(true, Ordering::Release);
}

/// Builds the flyout hidden, so the first click opens it at once.
pub fn prepare(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(PREPARE_AFTER).await;
        build(&app).await;
    });
}

/// Builds the flyout if it is not there, then shows it if a click asked for it meanwhile.
async fn build(app: &AppHandle) {
    if app.get_webview_window(LABEL).is_some() || BUILDING.swap(true, Ordering::AcqRel) {
        return;
    }
    READY.store(false, Ordering::Release);
    SHOWN.store(false, Ordering::Release);
    SUSPENDED.store(false, Ordering::Release);
    *WARM_SIZE.lock().unwrap() = None;
    let built = crate::app::on_demand_window(app, LABEL).await;
    if built.is_some() {
        let started = Instant::now();
        while !READY.load(Ordering::Acquire) && started.elapsed() < FIRST_PAINT {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Drawn before anything shows it, a click made meanwhile included.
        warm(app).await;
    }
    // Not suspended here: a webview suspended before it ever drew opens as an empty panel.
    // Ended on the main thread, where clicks arrive (`toggle`), so none falls between the two.
    let h = app.clone();
    let _ = app.run_on_main_thread(move || {
        BUILDING.store(false, Ordering::Release);
        show_if_asked(&h);
    });
}

/// Shows the flyout if a click asked for it while it was being built or drawn off-screen, once
/// neither is going on. Runs on the main thread, after whichever of the two ends last.
fn show_if_asked(app: &AppHandle) {
    if BUILDING.load(Ordering::Acquire) || WARMING.load(Ordering::Acquire) || app.get_webview_window(LABEL).is_none() {
        return;
    }
    if SHOW_WHEN_BUILT.swap(false, Ordering::AcqRel) {
        show(app);
    }
}

/// Before the flyout's first open, draws it again at a new size once that has held for
/// `WARM_SETTLE` (`warm`). An opened flyout keeps what it drew when it closes, and its page lays
/// out only while open, so nothing is needed after that.
pub fn keep_drawn(app: &AppHandle) {
    if SHOWN.load(Ordering::Acquire) {
        return;
    }
    let generation = WARM_GEN.fetch_add(1, Ordering::AcqRel) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(WARM_SETTLE).await;
        if WARM_GEN.load(Ordering::Acquire) == generation {
            warm(&app).await;
        }
    });
}

/// Draws the hidden, never opened flyout off-screen at its current size and waits until its
/// page has drawn, so its first open starts from that frame like every later one. Drawn again
/// should the page lay out to another size meanwhile; skipped once the flyout has been opened,
/// or while it is on screen (an agent's `inspect` draws it off-screen the same way).
async fn warm(app: &AppHandle) {
    let Some(win) = app.get_webview_window(LABEL) else { return };
    if WARMING.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut drew = true;
    for _ in 0..3 {
        let size = win.outer_size().ok().map(|s| (s.width, s.height));
        if !needs_drawing(SHOWN.load(Ordering::Acquire), *WARM_SIZE.lock().unwrap(), size) {
            break;
        }
        drew = draw_hidden(app, &win).await;
        if !drew {
            break;
        }
        *WARM_SIZE.lock().unwrap() = size;
    }
    let h = app.clone();
    let _ = app.run_on_main_thread(move || {
        WARMING.store(false, Ordering::Release);
        show_if_asked(&h);
        // A size reported while it was being drawn found it busy (`keep_drawn`): drawn at that
        // size next. Not after a drawing that failed, which would only fail again.
        let size = win.outer_size().ok().map(|s| (s.width, s.height));
        if drew && needs_drawing(SHOWN.load(Ordering::Acquire), *WARM_SIZE.lock().unwrap(), size) && !win.is_visible().unwrap_or(true) {
            keep_drawn(&h);
        }
    });
}

/// Whether the hidden flyout is to be drawn (`warm`): never opened yet, and not drawn at its
/// current window size (physical) already.
fn needs_drawing(opened: bool, drawn_at: Option<(u32, u32)>, size: Option<(u32, u32)>) -> bool {
    !opened && size.is_some() && drawn_at != size
}

/// Shows the hidden flyout off-screen beside its monitor (`inspect::show_off_screen`), asks its
/// page for a frame, waits for it (`WARM_DRAW` at most), then hides it again. False when it was not
/// drawn: it was on screen already (opened, or an agent's `inspect`), or the page did not answer.
#[cfg(windows)]
async fn draw_hidden(app: &AppHandle, win: &tauri::WebviewWindow) -> bool {
    use crate::inspect::{hide_again, on_main, show_off_screen, SendController};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    let Ok(hwnd) = win.hwnd() else { return false };
    // Kept as a number: a raw window handle cannot be held across an await.
    let hwnd = hwnd.0 as isize;
    let (tx, rx) = tokio::sync::oneshot::channel();
    if win.with_webview(move |w| { let _ = tx.send(SendController(w.controller())); }).is_err() {
        return false;
    }
    let Ok(controller) = rx.await else { return false };
    // Drawn as it will open: in the taskbar's theme.
    follow_taskbar_theme(win);
    DRAWN.store(false, Ordering::Release);
    // Checked where it is shown, on the main thread, so nothing shows it in between.
    let shown = on_main(win, {
        let controller = controller.clone();
        move || unsafe {
            let controller = controller;
            let hwnd = hwnd as HWND;
            (IsWindowVisible(hwnd) == 0).then(|| show_off_screen(hwnd, &controller.0))
        }
    })
    .await
    .ok()
    .flatten();
    let Some(saved) = shown else { return false };
    let started = Instant::now();
    let _ = app.emit_to(LABEL, "panel-warm", ());
    while !DRAWN.load(Ordering::Acquire) && started.elapsed() < WARM_DRAW {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The frame reaches the window just after the page has drawn it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = on_main(win, move || unsafe {
        let controller = controller;
        hide_again(hwnd as HWND, &controller.0, saved)
    })
    .await;
    DRAWN.load(Ordering::Acquire)
}

#[cfg(not(windows))]
async fn draw_hidden(_app: &AppHandle, _win: &tauri::WebviewWindow) -> bool {
    false
}

/// Set up a newly built flyout window: rounded, kept visible under Aero Peek, closed on blur.
pub fn watch(app: &AppHandle, win: &tauri::WebviewWindow) {
    #[cfg(windows)]
    {
        crate::dock::exclude_from_peek(win);
        round_corners(win);
    }
    let handle = app.clone();
    win.on_window_event(move |e| {
        if let WindowEvent::Focused(false) = e {
            on_blur(&handle);
        }
    });
}

/// Positions the flyout above the widget, clamped to the monitor's work area.
pub fn place(app: &AppHandle) {
    let Some(win) = app.get_webview_window(LABEL) else { return };
    let state = app.state::<AppState>();
    let height = *state.panel_height.lock().unwrap();
    let anchor = *state.panel_anchor.lock().unwrap();

    let monitor = anchor
        .and_then(|(x, y)| app.monitor_from_point(x, y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(m) = monitor else { return };
    let scale = m.scale_factor();
    let area = m.work_area();
    let (ax, ay, aw, ah) = (
        area.position.x as f64,
        area.position.y as f64,
        area.size.width as f64,
        area.size.height as f64,
    );
    // Never taller than the work area: with many accounts the flyout's top (its header and the
    // first provider) would otherwise sit off-screen. The account list scrolls instead.
    let height = height.min((ah / scale - 2.0 * MARGIN).max(200.0));
    let (w, h, margin) = (WIDTH * scale, height * scale, MARGIN * scale);
    let (anchor_x, anchor_y) = anchor.unwrap_or((ax + aw, ay + ah));

    let x = (anchor_x - w / 2.0).clamp(ax + margin, ax + aw - w - margin);
    // Taskbar at the bottom (the usual case) puts the widget below the work area's middle.
    let y = if anchor_y >= ay + ah / 2.0 { ay + ah - h - margin } else { ay + margin };

    let _ = win.set_size(LogicalSize::new(WIDTH, height));
    let _ = win.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32));
}

pub fn toggle(app: &AppHandle) {
    let Some(win) = app.get_webview_window(LABEL) else {
        // Not built yet (a click within moments of startup, or the webview went away): build it
        // and show it once its page has laid out.
        SHOW_WHEN_BUILT.store(true, Ordering::Release);
        let app = app.clone();
        tauri::async_runtime::spawn(async move { build(&app).await });
        return;
    };
    // Being built, or being drawn off-screen (where it is "visible"): shown once that is done.
    if BUILDING.load(Ordering::Acquire) || WARMING.load(Ordering::Acquire) {
        SHOW_WHEN_BUILT.store(true, Ordering::Release);
        return;
    }
    if win.is_visible().unwrap_or(false) {
        hide(app);
        return;
    }
    // Clicking the widget blurs (and hides) the flyout just before the click arrives.
    let just_hidden = app
        .state::<AppState>()
        .panel_hidden_at
        .lock()
        .unwrap()
        .is_some_and(|t| t.elapsed() < Duration::from_millis(250));
    if just_hidden {
        return;
    }
    show(app);
}

/// Flyouts follow the taskbar's theme ("Windows mode"), not the apps' one.
fn follow_taskbar_theme(win: &tauri::WebviewWindow) {
    let theme = if crate::dock::taskbar_is_light() { Theme::Light } else { Theme::Dark };
    let _ = win.set_theme(Some(theme));
}

fn show(app: &AppHandle) {
    let Some(win) = app.get_webview_window(LABEL) else { return };
    follow_taskbar_theme(&win);
    place(app);
    OPEN.store(true, Ordering::Release);
    let _ = win.show();
    set_webview_visible(&win, true);
    let _ = win.set_focus();
    let _ = app.emit("panel-visible", true);
}

fn hide(app: &AppHandle) {
    OPEN.store(false, Ordering::Release);
    if let Some(win) = app.get_webview_window(LABEL) {
        set_webview_visible(&win, false);
        let _ = win.hide();
    }
    *app.state::<AppState>().panel_hidden_at.lock().unwrap() = Some(Instant::now());
    let _ = app.emit("panel-visible", false);
}

/// Esc in the flyout, like the shell's own flyouts.
pub fn close(app: &AppHandle) {
    hide(app);
}

/// Losing focus closes an open flyout. A closed one reports focus changes too (as it is built,
/// and each time it is drawn off-screen): closing it then would only mark it closed just now, and
/// the click right after would be taken for the one that closed it (`toggle`) and do nothing.
pub fn on_blur(app: &AppHandle) {
    if OPEN.load(Ordering::Acquire) {
        hide(app);
    }
}

/// Tells the flyout's webview it is (or is no longer) on screen. Showing the window alone can leave
/// a webview that was built hidden, and stayed hidden, not drawing: the flyout then opens as an
/// empty panel. `inspect` does the same when it shows a hidden window off-screen.
///
/// Once it has been shown, hiding also suspends the webview, which lets WebView2 give back most of
/// its working set (about 450 MB down to 120 MB across the app's processes); showing resumes it first.
#[cfg(windows)]
fn set_webview_visible(win: &tauri::WebviewWindow, visible: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_3;
    use webview2_com::TrySuspendCompletedHandler;
    use windows::core::Interface;
    let _ = win.with_webview(move |w| unsafe {
        let controller = w.controller();
        let webview = controller.CoreWebView2().ok().and_then(|c| c.cast::<ICoreWebView2_3>().ok());
        if visible {
            if SUSPENDED.swap(false, Ordering::AcqRel) {
                if let Some(webview) = &webview {
                    let _ = webview.Resume();
                }
            }
            let _ = controller.SetIsVisible(true);
            let _ = controller.NotifyParentWindowPositionChanged();
            SHOWN.store(true, Ordering::Release);
        } else {
            let _ = controller.SetIsVisible(false);
            // Never before it has been on screen once: suspended before it ever drew, it opens empty.
            // A close hides more than once (blur, then the click); suspend once.
            if SHOWN.load(Ordering::Acquire) && !SUSPENDED.swap(true, Ordering::AcqRel) {
                if let Some(webview) = &webview {
                    let _ = webview.TrySuspend(&TrySuspendCompletedHandler::create(Box::new(|_, _| Ok(()))));
                }
            }
        }
    });
}

#[cfg(not(windows))]
fn set_webview_visible(_win: &tauri::WebviewWindow, _visible: bool) {}

/// Rounded corners like every Windows 11 flyout (undecorated windows are square by default).
#[cfg(windows)]
pub fn round_corners(window: &tauri::WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
    let Ok(hwnd) = window.hwnd() else { return };
    let value = DWMWCP_ROUND;
    unsafe {
        DwmSetWindowAttribute(
            hwnd.0 as _,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &value as *const _ as *const _,
            std::mem::size_of_val(&value) as u32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hidden_flyout_is_drawn_at_each_new_size_until_it_first_opens() {
        let (loading, accounts) = (Some((592, 135)), Some((592, 920)));
        assert!(needs_drawing(false, None, loading), "never drawn yet");
        assert!(!needs_drawing(false, loading, loading), "drawn at this size already");
        assert!(needs_drawing(false, loading, accounts), "laid out to another size since");
        assert!(!needs_drawing(true, None, accounts), "opened: closing keeps what it drew");
        assert!(!needs_drawing(true, loading, accounts), "opened: its page lays out only while open");
        assert!(!needs_drawing(false, None, None), "no window size to draw at");
    }

    /// The page's side of the first open (src/main.js): until then it lays out every change as it
    /// comes, so the hidden flyout has the accounts and the height they need; it reports heights
    /// added up from its parts, and not before the settings; and it answers `warm`. Read from the
    /// source, as no test runs the page.
    #[test]
    fn the_flyout_page_stays_current_until_its_first_open_and_answers_the_warm_up() {
        let js = include_str!("../../src/main.js").replace("\r\n", "\n");
        for needle in [
            // Deferred only once it has opened (a closed, opened flyout may be suspended)...
            "function deferWhileClosed() {\n  if (IS_DOCK || panelOpen || !opened) return false;",
            // ...which its first open marks.
            "if (payload) { opened = true; catchUp(); }",
            // A closed flyout still shows no notice, opened or not.
            "function showToast(text) {",
            "if (!IS_DOCK && !panelOpen) return;",
            // Heights: not before the settings (rows or cards), and added up from the parts.
            "else if (!current) return;",
            "invoke(\"set_panel_height\", { height: flyoutHeight()",
            // The drawn frame `warm` waits for: two animation frames.
            "listen(\"panel-warm\", () => requestAnimationFrame(() => requestAnimationFrame(() => invoke(\"panel_drawn\"))));",
        ] {
            assert!(js.contains(needle), "src/main.js no longer has: {needle}");
        }
        // And the app hears its answer: `panel_drawn` is among the commands it registers.
        let app = crate::source_guard::tokens(include_str!("app.rs"));
        let start = crate::source_guard::find(&app, "tauri::generate_handler![").expect("app.rs registers its commands with generate_handler!");
        let commands = &app[start..start + app[start..].find(']').expect("the command list closes")];
        assert!(commands.split(['[', ',']).any(|c| c == "panel_drawn"), "{commands}");
    }
}
