<#
.SYNOPSIS
  Clicks the running SlopBar's taskbar widget and checks that the flyout is really drawn on screen.

.DESCRIPTION
  Posts a click to the native widget window (class SlopBarTaskbarWidget), waits for the flyout
  to become visible, fronts it the way a real click would, raises it topmost and captures the
  screen where it is with BitBlt (SRCCOPY|CAPTUREBLT). A second click closes it, and the same
  rectangle is captured again. If the two captures match, nothing of the flyout reached the
  screen (a blank or transparent panel).

  The capture is compared with the flyout's own pixels: PrintWindow (PW_RENDERFULLCONTENT) has
  the flyout draw itself, web view included, into a bitmap. That rendering is only the
  reference, never the capture: it looks right even when nothing reaches the screen, which is
  exactly the bug the screen capture looks for (a flyout that is shown but never composited).

  The screen is also captured 50 ms after the flyout appears, and that early capture must show
  the flyout as the later one does. A flyout must be drawn when it appears, not a moment later:
  the first open after a start once showed an empty panel, then a grey and an almost black one,
  and was drawn only 0.3 to 0.5 s after the click, which the later capture alone usually missed.
  -AfterRestart <seconds> tests exactly that open: it restarts SlopBar through its agent door
  (slopbar-mcp restart_app), then clicks that many seconds after the new SlopBar started. That
  open must also keep its size for its first 250 ms: appearing at an earlier height and growing
  as it laid itself out was the other half of the same bug. (Growing later is right: accounts that
  arrive while it is open, as the first refresh after a start can. Elsewhere a size change as the
  flyout opens can be right too, an account added while it was closed, and only the early check is
  skipped.) The capture is taken while the flyout holds still. Windows on no monitor are not
  counted: the app draws its closed flyout off-screen before its first open.

  FAIL means: the flyout did not appear within -TimeoutMs (2 s), or the screen matches the
  flyout's own pixels on less than 90% of its client area, or other windows covered more than 2%
  of it (WindowFromPoint over a grid of its client area), or the capture matches what is behind it
  by more than 50%, or it has fewer than 10 distinct colours, or the early capture matches the
  later one on less than 30% of the flyout (drawn at once, it matches on 60% or more mid-way
  through its entrance animation; appearing undrawn, on 0 to 4%) or shows exactly what is behind
  it on more than half (a transparent, undrawn flyout). The colour checks alone once
  passed a capture that showed other windows where the flyout should have been. The PNGs (the
  capture, the early one, what was behind, and the flyout's own rendering) are written to -OutDir
  (%TEMP% by default), never into the repository.

  Run it in an unlocked, interactive desktop session. It refuses to run when there is no
  foreground window (a locked screen, a disconnected remote session): nothing composites then,
  and every capture is meaningless.

  Exit codes: 0 PASS, 1 FAIL, 2 could not run.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts/test-flyout-click.ps1

.EXAMPLE
  1..5 | % { powershell -ExecutionPolicy Bypass -File scripts/test-flyout-click.ps1 }

.EXAMPLE
  5, 30, 60 | % { powershell -ExecutionPolicy Bypass -File scripts/test-flyout-click.ps1 -AfterRestart $_ }
#>
param(
  [int]$WaitMs = 400,       # how long the flyout gets to draw before the capture
  [int]$TimeoutMs = 2000,   # how long it may take to become visible
  [string]$OutDir = $env:TEMP,
  [int]$AfterRestart = -1   # restart SlopBar first and click this many seconds after it started
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$source = @"
using System; using System.Text; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class FlyoutTest {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string t);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern bool AllowSetForegroundWindow(uint pid);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool f);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] public static extern IntPtr GetDC(IntPtr h);
  [DllImport("user32.dll")] public static extern int ReleaseDC(IntPtr h, IntPtr dc);
  [DllImport("gdi32.dll")] public static extern bool BitBlt(IntPtr d, int x, int y, int w, int h, IntPtr s, int sx, int sy, uint op);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr h, uint flags);
  // The share of a grid over the window's client area where it is the topmost window on screen
  // (the web view's child windows count as it). Another window in front lowers it.
  public static double OnTop(IntPtr h, int step) {
    RECT c; POINT o = new POINT();
    if (!GetClientRect(h, out c) || !ClientToScreen(h, ref o)) return 0;
    int n = 0, mine = 0;
    for (int y = c.T + step / 2; y < c.B; y += step)
      for (int x = c.L + step / 2; x < c.R; x += step) {
        POINT p = new POINT(); p.X = o.X + x; p.Y = o.Y + y; n++;
        if (GetAncestor(WindowFromPoint(p), 2) == h) mine++;   // GA_ROOT
      }
    return n == 0 ? 0 : (double)mine / n;
  }
  // A real click leaves the flyout in front with focus. Attaching to the foreground thread's
  // input lets this process hand the foreground over the same way.
  public static bool Front(IntPtr h) {
    uint p; uint fg = GetWindowThreadProcessId(GetForegroundWindow(), out p); uint me = GetCurrentThreadId();
    AttachThreadInput(me, fg, true); bool ok = SetForegroundWindow(h); AttachThreadInput(me, fg, false); return ok;
  }
  // The app's visible windows on a monitor. One shown but on no monitor is being drawn off-screen
  // (the closed flyout, before its first open), not on screen.
  public static List<IntPtr> Visible(uint pid, string skipClass) {
    var list = new List<IntPtr>();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p);
      if (p == pid && IsWindowVisible(h) && MonitorFromWindow(h, 0) != IntPtr.Zero) { var sb = new StringBuilder(256); GetClassName(h, sb, 256);
        if (sb.ToString() != skipClass) list.Add(h); } return true; }, IntPtr.Zero);
    return list;
  }
}
"@
try { Add-Type -TypeDefinition $source }
catch {
  # Windows PowerShell compiles in TEMP; one it cannot use (seen with C:\Windows\Temp) fails here.
  $own = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Temp\slopbar-flyout-test'
  New-Item -ItemType Directory -Force $own | Out-Null
  $env:TEMP = $own; $env:TMP = $own
  Add-Type -TypeDefinition $source
}
$T = [FlyoutTest]
$WidgetClass = 'SlopBarTaskbarWidget'
$WM_LBUTTONDOWN = 0x0201; $WM_LBUTTONUP = 0x0202
$SRCCOPY_CAPTUREBLT = 0x40CC0020
$HWND_TOPMOST = [IntPtr](-1); $HWND_NOTOPMOST = [IntPtr](-2)
$SWP_NOSIZE_NOMOVE_NOACTIVATE = 0x13

# Real pixels, like the app (per-monitor DPI aware v2), so window rectangles match the screen.
[void]$T::SetProcessDpiAwarenessContext([IntPtr](-4))

if ($T::GetForegroundWindow() -eq [IntPtr]::Zero) {
  Write-Host 'CANNOT RUN: there is no foreground window (locked screen or disconnected session). Nothing is composited, so any capture would be meaningless. Run this on an unlocked, interactive desktop.'
  exit 2
}
$widget = $T::FindWindow($WidgetClass, [NullString]::Value)
if ($widget -eq [IntPtr]::Zero) {
  Write-Host "CANNOT RUN: no $WidgetClass window. Is SlopBar running with the native taskbar widget on?"
  exit 2
}
[uint32]$appPid = 0; [void]$T::GetWindowThreadProcessId($widget, [ref]$appPid)

if ($AfterRestart -ge 0) {
  # restart_app through the agent door, as agents ask for it: SlopBar exits as from its menu and
  # starts again.
  $mcp = Join-Path $env:LOCALAPPDATA 'SlopBar\slopbar-mcp.exe'
  if (-not (Test-Path $mcp)) { Write-Host "CANNOT RUN: no $mcp to restart SlopBar through."; exit 2 }
  $info = New-Object Diagnostics.ProcessStartInfo $mcp
  $info.UseShellExecute = $false; $info.CreateNoWindow = $true
  $info.RedirectStandardInput = $true; $info.RedirectStandardOutput = $true
  $door = [Diagnostics.Process]::Start($info)
  $door.StandardInput.WriteLine('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test-flyout-click","version":"1"}}}')
  $door.StandardInput.WriteLine('{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"restart_app","arguments":{}}}')
  $door.StandardInput.Close(); $answer = $door.StandardOutput.ReadToEndAsync()
  if (-not $door.WaitForExit(30000)) { $door.Kill(); Write-Host 'CANNOT RUN: the agent door did not answer restart_app within 30 s.'; exit 2 }
  [void]$answer.Wait(5000)
  # The new SlopBar's widget, then the wait from when it started.
  $restart = [Diagnostics.Stopwatch]::StartNew()
  [uint32]$newPid = $appPid
  while ($newPid -eq $appPid -and $restart.Elapsed.TotalSeconds -lt 60) {
    Start-Sleep -Milliseconds 100
    $widget = $T::FindWindow($WidgetClass, [NullString]::Value)
    [uint32]$found = 0
    if ($widget -ne [IntPtr]::Zero) { [void]$T::GetWindowThreadProcessId($widget, [ref]$found) }
    if ($found -ne 0) { $newPid = $found }
  }
  if ($newPid -eq $appPid) { Write-Host 'CANNOT RUN: SlopBar did not start again within 60 s of restart_app.'; exit 2 }
  $appPid = $newPid
  $started = (Get-Process -Id $appPid).StartTime
  $left = ($started.AddSeconds($AfterRestart) - (Get-Date)).TotalMilliseconds
  if ($left -gt 0) { Start-Sleep -Milliseconds ([int]$left) }
  Write-Host ("restarted:       clicked {0:N1} s after SlopBar started" -f ((Get-Date) - $started).TotalSeconds)
}

function Click-Widget {
  $lp = [IntPtr]((10 -shl 16) -bor 10)
  [void]$T::PostMessage($widget, $WM_LBUTTONDOWN, [IntPtr]1, $lp)
  [void]$T::PostMessage($widget, $WM_LBUTTONUP, [IntPtr]0, $lp)
}
# What the flyout draws itself (PW_RENDERFULLCONTENT renders its web view too), as the reference
# for the screen capture; $null when Windows cannot render it.
function Own([IntPtr]$h, [FlyoutTest+RECT]$r) {
  $bmp = New-Object Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
  $g = [Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc()
  $ok = $T::PrintWindow($h, $hdc, 2)
  $g.ReleaseHdc($hdc); $g.Dispose()
  if ($ok) { return $bmp }
  $bmp.Dispose(); return $null
}
function Capture([FlyoutTest+RECT]$r) {
  $w = $r.R - $r.L; $h = $r.B - $r.T
  $bmp = New-Object Drawing.Bitmap $w, $h
  $g = [Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc(); $screen = $T::GetDC([IntPtr]::Zero)
  [void]$T::BitBlt($hdc, 0, 0, $w, $h, $screen, $r.L, $r.T, $SRCCOPY_CAPTUREBLT)
  [void]$T::ReleaseDC([IntPtr]::Zero, $screen); $g.ReleaseHdc($hdc); $g.Dispose()
  $bmp
}

$before = $T::Visible($appPid, $WidgetClass)
[void]$T::AllowSetForegroundWindow($appPid)   # a real click lets the app take the foreground
$sw = [Diagnostics.Stopwatch]::StartNew()
Click-Widget
$flyout = [IntPtr]::Zero
while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
  $new = $T::Visible($appPid, $WidgetClass) | Where-Object { $before -notcontains $_ }
  if ($new) { $flyout = @($new)[0]; break }
  Start-Sleep -Milliseconds 10
}
$visibleMs = $sw.ElapsedMilliseconds
if ($flyout -eq [IntPtr]::Zero) {
  Write-Host "FAIL: the flyout did not become visible within $TimeoutMs ms. (If it was already open, the click closed it: close it and run again.)"
  exit 1
}

$fronted = $T::Front($flyout)
# What the screen shows a few frames after the flyout appeared: it must already be drawn.
$untilEarly = 50 - ($sw.ElapsedMilliseconds - $visibleMs)
if ($untilEarly -gt 0) { Start-Sleep -Milliseconds $untilEarly }
$earlyMs = $sw.ElapsedMilliseconds - $visibleMs
$er = New-Object FlyoutTest+RECT; [void]$T::GetWindowRect($flyout, [ref]$er)
$early = Capture $er
# Its size a moment later: one that grows this soon laid itself out on screen as it opened.
$untilSettled = 250 - ($sw.ElapsedMilliseconds - $visibleMs)
if ($untilSettled -gt 0) { Start-Sleep -Milliseconds $untilSettled }
$mr = New-Object FlyoutTest+RECT; [void]$T::GetWindowRect($flyout, [ref]$mr)
Start-Sleep -Milliseconds ([Math]::Max(0, $WaitMs - ($sw.ElapsedMilliseconds - $visibleMs)))
# Captured while it holds still: accounts that arrive as it opens make it grow, and a capture taken
# across that compares the wrong pixels.
for ($try = 1; $try -le 3; $try++) {
  $r = New-Object FlyoutTest+RECT; [void]$T::GetWindowRect($flyout, [ref]$r)
  [void]$T::SetWindowPos($flyout, $HWND_TOPMOST, 0, 0, 0, 0, $SWP_NOSIZE_NOMOVE_NOACTIVATE)
  Start-Sleep -Milliseconds 150
  $onTop = $T::OnTop($flyout, 16)
  $shot = Capture $r
  $shownAtShot = $T::IsWindowVisible($flyout)
  $own = Own $flyout $r
  # The client area within the captured window rectangle (its invisible borders show what is behind).
  $cr = New-Object FlyoutTest+RECT; [void]$T::GetClientRect($flyout, [ref]$cr)
  $co = New-Object FlyoutTest+POINT; [void]$T::ClientToScreen($flyout, [ref]$co)
  $cx = $co.X - $r.L; $cy = $co.Y - $r.T
  [void]$T::SetWindowPos($flyout, $HWND_NOTOPMOST, 0, 0, 0, 0, $SWP_NOSIZE_NOMOVE_NOACTIVATE)
  $after = New-Object FlyoutTest+RECT; [void]$T::GetWindowRect($flyout, [ref]$after)
  $still = $after.L -eq $r.L -and $after.T -eq $r.T -and $after.R -eq $r.R -and $after.B -eq $r.B -and $cx -ge 0 -and $cy -ge 0
  if ($still -or $try -eq 3) { break }
  $shot.Dispose(); if ($own) { $own.Dispose() }
  Start-Sleep -Milliseconds 300
}
if (-not $still) { Write-Host 'CANNOT RUN: the flyout kept moving or changing size while it was captured.'; exit 2 }

# A second click closes it, as for a user; then capture what was behind it.
Start-Sleep -Milliseconds 300
Click-Widget
$closeWatch = [Diagnostics.Stopwatch]::StartNew()
while ($T::IsWindowVisible($flyout) -and $closeWatch.ElapsedMilliseconds -lt 2000) { Start-Sleep -Milliseconds 10 }
$closed = -not $T::IsWindowVisible($flyout)
Start-Sleep -Milliseconds 500   # let the close animation finish
$behind = Capture $r

# Sample a grid: distinct colours (to 4 bits a channel) and exact matches with what is behind.
$colors = @{}; $same = 0; $n = 0
for ($y = 0; $y -lt $shot.Height; $y += 6) {
  for ($x = 0; $x -lt $shot.Width; $x += 6) {
    $a = $shot.GetPixel($x, $y).ToArgb(); $n++
    $colors[$a -band 0xF0F0F0] = 1
    if ($a -eq $behind.GetPixel($x, $y).ToArgb()) { $same++ }
  }
}
$match = if ($n) { $same / $n } else { 1 }
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$shotPath = Join-Path $OutDir "slopbar-flyout-$stamp.png"
$behindPath = Join-Path $OutDir "slopbar-flyout-$stamp-behind.png"
# How much of the client area shows on screen what the flyout draws itself.
$agree = 0; $m = 0
if ($own) {
  for ($y = $cy; $y -lt [Math]::Min($cy + $cr.B, $shot.Height); $y += 6) {
    for ($x = $cx; $x -lt [Math]::Min($cx + $cr.R, $shot.Width); $x += 6) {
      $a = $shot.GetPixel($x, $y); $b = $own.GetPixel($x, $y); $m++
      if ([Math]::Abs($a.R - $b.R) + [Math]::Abs($a.G - $b.G) + [Math]::Abs($a.B - $b.B) -le 48) { $agree++ }
    }
  }
}
$drawn = if ($m) { $agree / $m } else { 0 }
# How much of the client area the early capture showed as the later one does. Only when the
# flyout kept its size: one that grew as it opened (an account added while it was closed) shows
# its earlier layout first, which is not a flyout appearing undrawn.
# Also how much of it was exactly what is behind the flyout: a transparent, undrawn flyout over a
# window of similar colours can come close to the later pixels, never to the very same as behind.
$drawnEarly = $null; $behindEarly = $null
if (($er.R - $er.L) -eq ($r.R - $r.L) -and ($er.B - $er.T) -eq ($r.B - $r.T)) {
  $agreeE = 0; $sameE = 0; $mE = 0
  for ($y = $cy; $y -lt [Math]::Min($cy + $cr.B, $shot.Height); $y += 6) {
    for ($x = $cx; $x -lt [Math]::Min($cx + $cr.R, $shot.Width); $x += 6) {
      $a = $early.GetPixel($x, $y); $b = $shot.GetPixel($x, $y); $mE++
      if ([Math]::Abs($a.R - $b.R) + [Math]::Abs($a.G - $b.G) + [Math]::Abs($a.B - $b.B) -le 48) { $agreeE++ }
      if ($a.ToArgb() -eq $behind.GetPixel($x, $y).ToArgb()) { $sameE++ }
    }
  }
  $drawnEarly = if ($mE) { $agreeE / $mE } else { 0 }
  $behindEarly = if ($mE) { $sameE / $mE } else { 1 }
}
$earlyPath = Join-Path $OutDir "slopbar-flyout-$stamp-early.png"
$early.Save($earlyPath, [Drawing.Imaging.ImageFormat]::Png); $early.Dispose()
$shot.Save($shotPath, [Drawing.Imaging.ImageFormat]::Png); $behind.Save($behindPath, [Drawing.Imaging.ImageFormat]::Png)
$ownPath = Join-Path $OutDir "slopbar-flyout-$stamp-own.png"
if ($own) { $own.Save($ownPath, [Drawing.Imaging.ImageFormat]::Png); $own.Dispose() }
$shot.Dispose(); $behind.Dispose()

$reasons = @()
if (-not $own) { $reasons += "the flyout's own rendering could not be read (PrintWindow failed)" }
elseif ($drawn -lt 0.9) { $reasons += ("the screen shows the flyout's own pixels on only {0:P0} of it" -f $drawn) }
if ($onTop -lt 0.98) { $reasons += ('other windows covered {0:P0} of the flyout when it was captured' -f (1 - $onTop)) }
if ($match -gt 0.5) { $reasons += 'the capture matches what is behind the flyout by more than 50%' }
# Measured: the almost black frame of an undrawn first open showed 8; the flyout shows 15 while
# it looks for accounts just after a start (a right picture, which 20 failed) and 50-70 with them.
if ($colors.Count -lt 10) { $reasons += 'the capture has fewer than 10 distinct colours' }
$grew = ($er.R - $er.L) -ne ($mr.R - $mr.L) -or ($er.B - $er.T) -ne ($mr.B - $mr.T)
if ($AfterRestart -ge 0 -and $grew) {
  $reasons += ("the first open after a start changed size as it opened ({0}x{1}, then {2}x{3} 250 ms after it appeared): it appeared at an earlier height" -f ($er.R - $er.L), ($er.B - $er.T), ($mr.R - $mr.L), ($mr.B - $mr.T))
}
if ($null -ne $drawnEarly -and ($drawnEarly -lt 0.3 -or $behindEarly -gt 0.5)) {
  $reasons += ("the flyout appeared undrawn: {0} ms after it appeared, the screen showed its later pixels on only {1:P0} of it, and what is behind it on {2:P0}" -f $earlyMs, $drawnEarly, $behindEarly)
}
Write-Host ("visible after:   {0} ms" -f $visibleMs)
$class = New-Object Text.StringBuilder 256; [void]$T::GetClassName($flyout, $class, 256)
Write-Host ("flyout:          {0}x{1} at {2},{3}; fronted: {4}; class {5}; shown when captured: {6}" -f ($r.R - $r.L), ($r.B - $r.T), $r.L, $r.T, $fronted, $class, $shownAtShot)
if ($null -ne $drawnEarly) { Write-Host ("drawn at once:   {0:P0} of its area {1} ms after it appeared ({2:P0} as behind it)" -f $drawnEarly, $earlyMs, $behindEarly) }
else { Write-Host ("drawn at once:   not checked: it changed size as it opened ({0}x{1} at first)" -f ($er.R - $er.L), ($er.B - $er.T)) }
Write-Host ("own pixels:      {0:P0} of its area" -f $drawn)
Write-Host ("on top:          {0:P0} of its area" -f $onTop)
Write-Host ("distinct colours: {0}" -f $colors.Count)
Write-Host ("match behind:    {0:P0}" -f $match)
if (-not $closed) { Write-Host 'warning:         the flyout was still visible after the closing click, so the behind capture includes it' }
Write-Host "shot:            $shotPath"
Write-Host "early:           $earlyPath"
Write-Host "behind:          $behindPath"
if ($own) { Write-Host "own:             $ownPath" }
if ($reasons) { Write-Host ('FAIL: ' + ($reasons -join '; ')); exit 1 }
Write-Host 'PASS'
exit 0
