//! Lets an agent see a SlopBar window without the user's screen: the window's visible text and a
//! PNG drawn by WebView2 itself. A hidden window is shown off-screen without activation and
//! without a taskbar button, then hidden again, so nothing moves under the user and focus stays
//! where it was. A Settings page chosen for the look is switched back afterwards.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::dock::Rect;
use crate::door::SETTINGS_PAGES as PAGES;
/// Longest text returned; the pages are far shorter.
const MAX_TEXT: usize = 20_000;
/// Captures kept on disk; older ones are removed.
const KEEP: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Settings,
    /// The flyout that opens from the taskbar widget.
    Flyout,
    /// The quick switcher.
    Switcher,
    /// The taskbar widget.
    Widget,
}

impl Target {
    fn label(self) -> &'static str {
        match self {
            Target::Settings => "settings",
            Target::Flyout => crate::panel::LABEL,
            Target::Switcher => crate::app::SWITCHER_LABEL,
            Target::Widget => crate::dock::LABEL,
        }
    }
}

/// The native widget has no webview to ask: its text and picture come from the code that draws
/// it on the taskbar (`widget_win::inspect`).
#[cfg(windows)]
fn inspect_native_widget(app: &AppHandle, screenshot: bool) -> Result<Inspection, String> {
    let (text, png, was_visible) = crate::widget_win::inspect(app)?;
    let screenshot = if screenshot {
        let dir = captures_dir().ok_or("no app data folder")?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("SlopBar-widget-{}.png", chrono::Utc::now().format("%Y%m%d-%H%M%S%3f")));
        std::fs::write(&path, png).map_err(|e| e.to_string())?;
        prune(&dir);
        Some(path)
    } else {
        None
    };
    Ok(Inspection { window: Target::Widget, page: None, was_visible, text, screenshot })
}

#[derive(Debug, Serialize)]
pub struct Inspection {
    pub window: Target,
    pub page: Option<String>,
    /// Whether the user had the window open; if not, it was drawn off-screen and hidden again.
    pub was_visible: bool,
    pub text: String,
    pub screenshot: Option<PathBuf>,
}

/// Where captures are written, beside diagnostics exports.
pub fn captures_dir() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("diagnostics").join("captures"))
}

#[cfg(windows)]
pub async fn inspect(app: &AppHandle, target: Target, page: Option<String>, screenshot: bool) -> Result<Inspection, String> {
    if let Some(p) = page.as_deref() {
        if target != Target::Settings || !PAGES.contains(&p) {
            return Err(format!("page is only for the settings window, one of {PAGES:?}"));
        }
    }
    if target == Target::Widget && crate::widget_win::running() && crate::dock::native_wanted(app) {
        return inspect_native_widget(app, screenshot);
    }
    // Settings and the switcher exist only while open: one that is closed is created for the
    // look, given a moment to load, and destroyed again afterwards. (The web widget is never
    // built just for a look: it exists whenever it is the widget in use.)
    let (window, created) = match app.get_webview_window(target.label()) {
        Some(w) => (w, false),
        None if target != Target::Widget && crate::app::ON_DEMAND.contains(&target.label()) => {
            let w = crate::app::on_demand_window(app, target.label()).await.ok_or("window not available")?;
            (w, true)
        }
        None => return Err("window not available".into()),
    };
    let result = look(&window, target, page, screenshot, created).await;
    // Unless the user opened it meanwhile.
    if created && !window.is_visible().unwrap_or(false) {
        let _ = window.destroy();
    }
    result
}

#[cfg(windows)]
async fn look(window: &tauri::WebviewWindow, target: Target, page: Option<String>, screenshot: bool, created: bool) -> Result<Inspection, String> {
    use windows_sys::Win32::UI::WindowsAndMessaging as wm;
    let window = window.clone();
    // Kept as a number: a raw window handle cannot be held across an await.
    let hwnd = window.hwnd().map_err(|e| e.to_string())?.0 as isize;
    let controller = {
        let (tx, rx) = tokio::sync::oneshot::channel();
        window.with_webview(move |w| { let _ = tx.send(SendController(w.controller())); }).map_err(|e| e.to_string())?;
        rx.await.map_err(|_| "webview not available")?
    };
    if created {
        // The page loads its data as it starts (Settings fetches several reports and marks
        // `data-loading` on its root until they have settled; see settings.js).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let settled = "document.readyState === 'complete' && document.documentElement.dataset.loading !== '1'";
        while script(&window, &controller, settled).await.as_deref() != Ok("true") && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Then a moment to paint (longer for windows that do not mark their loads).
        tokio::time::sleep(Duration::from_millis(if target == Target::Settings { 300 } else { 800 })).await;
    }
    // Checked where it is shown, on the main thread, so nothing else shows it in between (the
    // flyout draws itself off-screen the same way before its first open: `panel::warm`).
    let restore = on_main(&window, {
        let controller = controller.clone();
        move || unsafe {
            let controller = controller;
            let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
            (wm::IsWindowVisible(hwnd) == 0).then(|| show_off_screen(hwnd, &controller.0))
        }
    }).await?;
    let was_visible = restore.is_none();
    if !was_visible {
        // A webview that was hidden needs a moment to lay out and paint again.
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    let result = async {
        let previous = match &page {
            Some(p) => {
                let before = script(&window, &controller, "document.querySelector('.nav.active')?.dataset.page ?? null").await?;
                let found = script(&window, &controller, &format!("(() => {{ const b = document.querySelector('.nav[data-page={}]'); b?.click(); return !!b; }})()", serde_json::to_string(p).unwrap())).await?;
                if found != "true" { return Err("that page is not in this build's Settings window".to_string()); }
                // Pages load their data on open (the Subscriptions report is fetched then).
                tokio::time::sleep(Duration::from_millis(800)).await;
                serde_json::from_str::<Option<String>>(&before).ok().flatten()
            }
            None => None,
        };
        let text = script(&window, &controller, "(document.querySelector('main .page.active') ?? document.body).innerText").await?;
        let mut text: String = serde_json::from_str(&text).unwrap_or_default();
        if text.chars().count() > MAX_TEXT { text = text.chars().take(MAX_TEXT).collect(); }
        let shot = if screenshot { Some(capture(&window, &controller, target, page.as_deref()).await?) } else { None };
        if let (Some(before), Some(now)) = (previous, page.as_deref()) {
            if before != now {
                let _ = script(&window, &controller, &format!("document.querySelector('.nav[data-page={}]')?.click()", serde_json::to_string(&before).unwrap())).await;
            }
        }
        Ok((text, shot))
    }.await;
    if let Some(saved) = restore {
        let controller = controller.clone();
        let _ = on_main(&window, move || unsafe { let controller = controller; hide_again(hwnd as windows_sys::Win32::Foundation::HWND, &controller.0, saved) }).await;
    }
    let (text, screenshot) = result?;
    Ok(Inspection { window: target, page, was_visible, text, screenshot })
}

#[cfg(not(windows))]
pub async fn inspect(_: &AppHandle, _: Target, _: Option<String>, _: bool) -> Result<Inspection, String> {
    Err("window inspection is only available on Windows".into())
}

/// The webview's controller. It is only ever called on the main thread (`on_main`); other threads
/// just carry it there.
#[cfg(windows)]
#[derive(Clone)]
pub(crate) struct SendController(pub(crate) webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller);
#[cfg(windows)]
unsafe impl Send for SendController {}
#[cfg(windows)]
unsafe impl Sync for SendController {}

/// Where a window shown off-screen was, and the extended-style bits `show_off_screen` changed,
/// for `hide_again`.
#[cfg(windows)]
pub(crate) struct Saved { x: i32, y: i32, added: isize, removed: isize }

/// Runs `f` on the main thread, where the window and its webview live.
#[cfg(windows)]
pub(crate) async fn on_main<T: Send + 'static>(window: &tauri::WebviewWindow, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    window.run_on_main_thread(move || { let _ = tx.send(f()); }).map_err(|e| e.to_string())?;
    rx.await.map_err(|_| "main thread unavailable".to_string())
}

/// Where a window (`window`, on monitor `on`) can be shown unseen: just outside that monitor, on
/// the first side (left, right, below) where it covers no monitor and is nearer to `on` than to
/// any other. A window on no monitor takes the nearest one's DPI, so it is drawn at the scale it
/// has where it opens; left of every monitor, it took the leftmost one's, which differs on a
/// laptop beside an external screen. Not above: a window that grows there (the flyout's height
/// can change while it is drawn) grows down into the monitor. None when no side qualifies.
pub(crate) fn off_screen_spot(window: Rect, on: Rect, monitors: &[Rect]) -> Option<(i32, i32)> {
    const GAP: i32 = 100;
    let (w, h) = (window.right - window.left, window.bottom - window.top);
    let at = |x: i32, y: i32| Rect { left: x, top: y, right: x + w, bottom: y + h };
    let overlaps = |a: Rect, b: &Rect| a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
    // As Windows picks the monitor of a window on none: the nearest.
    let gap = |a: Rect, m: &Rect| {
        let dx = i64::from((m.left - a.right).max(a.left - m.right).max(0));
        let dy = i64::from((m.top - a.bottom).max(a.top - m.bottom).max(0));
        dx * dx + dy * dy
    };
    [(on.left - w - GAP, window.top), (on.right + GAP, window.top), (window.left, on.bottom + GAP)]
        .into_iter()
        .find(|&(x, y)| {
            let spot = at(x, y);
            !monitors.iter().any(|m| overlaps(spot, m)) && monitors.iter().min_by_key(|m| gap(spot, m)) == Some(&on)
        })
}

/// Every monitor's rectangle, and the one `hwnd` is on (or nearest to).
#[cfg(windows)]
unsafe fn monitors_of(hwnd: windows_sys::Win32::Foundation::HWND) -> Option<(Rect, Vec<Rect>)> {
    use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    unsafe extern "system" fn add(_: HMONITOR, _: HDC, rect: *mut RECT, list: LPARAM) -> BOOL {
        let (r, list) = (&*rect, &mut *(list as *mut Vec<Rect>));
        list.push(Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom });
        1
    }
    let mut all: Vec<Rect> = Vec::new();
    EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(add), &mut all as *mut Vec<Rect> as LPARAM);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info) == 0 {
        return None;
    }
    let m = info.rcMonitor;
    Some((Rect { left: m.left, top: m.top, right: m.right, bottom: m.bottom }, all))
}

/// Whether `hwnd` is shown but on no monitor: drawn off-screen right now (`show_off_screen`).
#[cfg(windows)]
pub(crate) fn off_screen(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    unsafe { IsWindowVisible(hwnd) != 0 && MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL).is_null() }
}

/// Shows a hidden window off-screen beside its monitor (`off_screen_spot`), without activating it
/// or giving it a taskbar button, and tells its webview it is on screen, so it lays out and draws.
/// `hide_again` undoes it.
#[cfg(windows)]
pub(crate) unsafe fn show_off_screen(hwnd: windows_sys::Win32::Foundation::HWND, controller: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller) -> Saved {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging as wm;
    let mut rect: RECT = std::mem::zeroed();
    wm::GetWindowRect(hwnd, &mut rect);
    let ex_style = wm::GetWindowLongPtrW(hwnd, wm::GWL_EXSTYLE);
    // A tool window gets no taskbar button, so the taskbar does not flicker either. Only the bits
    // changed here are put back, so a style the app sets meanwhile is kept.
    let wanted = wm::WS_EX_TOOLWINDOW as isize | wm::WS_EX_NOACTIVATE as isize;
    let (added, removed) = (wanted & !ex_style, wm::WS_EX_APPWINDOW as isize & ex_style);
    wm::SetWindowLongPtrW(hwnd, wm::GWL_EXSTYLE, (ex_style | added) & !removed);
    let window = Rect { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom };
    let width = rect.right - rect.left;
    let (left, top) = monitors_of(hwnd)
        .and_then(|(on, all)| off_screen_spot(window, on, &all))
        // Left of every monitor, far enough that the window stays off them should taking another
        // monitor's DPI there make it up to three times as wide.
        .unwrap_or((wm::GetSystemMetrics(wm::SM_XVIRTUALSCREEN) - 3 * width - 100, rect.top));
    wm::SetWindowPos(hwnd, std::ptr::null_mut(), left, top, 0, 0, wm::SWP_NOSIZE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE);
    wm::ShowWindow(hwnd, wm::SW_SHOWNOACTIVATE);
    let _ = controller.SetIsVisible(true);
    Saved { x: rect.left, y: rect.top, added, removed }
}

#[cfg(windows)]
pub(crate) unsafe fn hide_again(hwnd: windows_sys::Win32::Foundation::HWND, controller: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller, saved: Saved) {
    use windows_sys::Win32::UI::WindowsAndMessaging as wm;
    let _ = controller.SetIsVisible(false);
    wm::ShowWindow(hwnd, wm::SW_HIDE);
    wm::SetWindowPos(hwnd, std::ptr::null_mut(), saved.x, saved.y, 0, 0, wm::SWP_NOSIZE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE);
    let ex_style = wm::GetWindowLongPtrW(hwnd, wm::GWL_EXSTYLE);
    wm::SetWindowLongPtrW(hwnd, wm::GWL_EXSTYLE, (ex_style & !saved.added) | saved.removed);
}

/// Runs a script in the page and returns its result as JSON text.
#[cfg(windows)]
async fn script(window: &tauri::WebviewWindow, controller: &SendController, js: &str) -> Result<String, String> {
    use webview2_com::ExecuteScriptCompletedHandler;
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let controller = controller.clone();
    let js = js.to_string();
    on_main(window, move || unsafe {
        let controller = controller;
        let run = || -> windows::core::Result<()> {
            let webview = controller.0.CoreWebView2()?;
            webview.ExecuteScript(&windows::core::HSTRING::from(js), &ExecuteScriptCompletedHandler::create(Box::new(move |status, json| {
                let _ = tx.send(status.map(|_| json).map_err(|e| e.to_string()));
                Ok(())
            })))
        };
        run().map_err(|e| e.to_string())
    }).await??;
    tokio::time::timeout(Duration::from_secs(10), rx).await.map_err(|_| "script timed out")?.map_err(|_| "script dropped")?
}

/// Writes the webview's current picture to a new PNG under `captures_dir`.
#[cfg(windows)]
async fn capture(window: &tauri::WebviewWindow, controller: &SendController, target: Target, page: Option<&str>) -> Result<PathBuf, String> {
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use windows::Win32::System::Com::{STGM_CREATE, STGM_WRITE};
    let dir = captures_dir().ok_or("no app data folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = format!("SlopBar-{}{}-{}.png", target.label(), page.map(|p| format!("-{p}")).unwrap_or_default(), chrono::Utc::now().format("%Y%m%d-%H%M%S%3f"));
    let path = dir.join(name);
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let controller = controller.clone();
    let file = path.clone();
    on_main(window, move || unsafe {
        let controller = controller;
        let run = || -> windows::core::Result<()> {
            let webview = controller.0.CoreWebView2()?;
            let stream = windows::Win32::UI::Shell::SHCreateStreamOnFileEx(&windows::core::HSTRING::from(file.as_os_str()), (STGM_CREATE | STGM_WRITE).0, 0, true, None)?;
            let held = stream.clone();
            webview.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &CapturePreviewCompletedHandler::create(Box::new(move |status| {
                // Releasing the stream closes the file.
                drop(held);
                let _ = tx.send(status.map_err(|e| e.to_string()));
                Ok(())
            })))
        };
        run().map_err(|e| e.to_string())
    }).await??;
    tokio::time::timeout(Duration::from_secs(10), rx).await.map_err(|_| "capture timed out")?.map_err(|_| "capture dropped")??;
    prune(&dir);
    Ok(path)
}

/// Keeps the newest `KEEP` captures in `dir`; other files are left alone.
pub fn prune(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut shots: Vec<_> = entries.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("SlopBar-") && e.file_name().to_string_lossy().ends_with(".png"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    shots.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in shots.into_iter().skip(KEEP) { let _ = std::fs::remove_file(path); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    const fn r(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect { left, top, right, bottom }
    }

    #[test]
    fn a_window_is_shown_unseen_beside_its_own_monitor() {
        // A flyout above the taskbar of a 2560x1440 monitor (ULTRA-MAGNUS's layout: two side by side).
        let flyout = r(2000, 500, 2592, 1420);
        let (left, right) = (r(0, 0, 2731, 1440), r(2731, 0, 5291, 1440));
        // On the left one: left of it, where nothing is.
        assert_eq!(off_screen_spot(flyout, left, &[left, right]), Some((-692, 500)));
        // On the right one: not left of it (that is the other monitor, on screen), but right of it.
        let there = r(4000, 500, 4592, 1420);
        assert_eq!(off_screen_spot(there, right, &[left, right]), Some((5391, 500)));
        // A middle monitor with screens on both sides: below it.
        let middle = r(2731, 0, 5291, 1440);
        let third = r(5291, 0, 7851, 1440);
        assert_eq!(off_screen_spot(there, middle, &[left, middle, third]), Some((4000, 1540)));
        // Screens left, right and below, nothing above: never above (a window growing there grows
        // into the monitor), so nowhere.
        let top_row = r(2731, 0, 5291, 1440);
        let sides = [r(0, 0, 2731, 1440), top_row, r(5291, 0, 8022, 1440), r(2731, 1440, 5291, 2880)];
        assert_eq!(off_screen_spot(there, top_row, &sides), None);
        // One monitor: left of it.
        assert_eq!(off_screen_spot(flyout, left, &[left]), Some((-692, 500)));
    }

    #[test]
    fn an_unseen_spot_is_nearer_its_own_monitor_than_any_other() {
        // Left of the laptop is free, but an external screen sits just below that spot (60 px from
        // it, the laptop 100 px), so a window there would take the external screen's DPI.
        let laptop = r(0, 0, 1920, 1080);
        let external = r(-2600, 1100, 0, 2540);
        let flyout = r(1300, 400, 1870, 1040);
        let spot = off_screen_spot(flyout, laptop, &[laptop, external]).expect("a side qualifies");
        assert_eq!(spot, (1920 + 100, 400), "right of the laptop, not left of it nor below it");
    }

    #[test]
    fn only_the_newest_captures_are_kept_and_other_files_are_left_alone() {
        let dir = TempDir::new();
        for i in 0..13 {
            std::fs::write(dir.0.join(format!("SlopBar-settings-{i:02}.png")), b"png").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        std::fs::write(dir.0.join("notes.txt"), b"keep").unwrap();
        prune(&dir.0);
        let mut left: Vec<String> = std::fs::read_dir(&dir.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left.len(), KEEP + 1);
        assert!(left.contains(&"notes.txt".to_string()) && left.contains(&"SlopBar-settings-12.png".to_string()));
        assert!(!left.contains(&"SlopBar-settings-00.png".to_string()));
    }
}
