<#
.SYNOPSIS
  Shows on a clean Windows machine that Add account finds Claude Code where SlopBar looks for it,
  and says how to install it where there is none.

.DESCRIPTION
  Drives the given SlopBar through its door (slopbar-mcp.exe --call), as an agent does, and checks
  what actually runs, by process:
    S0 nothing     No Claude Code anywhere SlopBar looks: add_account fails, its message names the
                   PowerShell line that installs Claude Code, and nothing starts.
    S1 unverified  A copy laid out as the Claude desktop app keeps its own
                   (%APPDATA%\Claude\claude-code\<version>\<id>\claude.exe), but without the app's
                   .verified marker: still nothing starts.
    S2 desktop     With .verified: add_account starts `claude ... auth login` from that very file, in
                   the sign-in's console (cmd running login.cmd) under SlopBar, with no restart, and
                   the sign-in is still waiting 15 s later.
    S3 new PATH    A claude.cmd in a folder added to the user PATH in the registry after SlopBar
                   started, as an installer adds one: add_account runs it (no restart), and once that
                   sign-in is ended, so does SlopBar's logout of it.
    S4 installer's folder
                   Claude Code in %USERPROFILE%\.local\bin, which no PATH names (Anthropic's installer
                   can leave it off): add_account starts `claude ... auth login` from there, ahead of
                   the desktop app's copy.
  Each sign-in is ended unfinished (only what SlopBar started is stopped); nothing is signed in.

  SlopBar runs as the user would run it: when it is not running, it is started through Explorer
  from an administrator window (a runner's step), so it runs on the desktop as the user.

  It changes the user PATH in the registry (and puts it back) and quits SlopBar at the end, so it
  runs only on a CI runner (GITHUB_ACTIONS=true), never on a computer someone uses.

  Exit codes: 0 PASS, 1 FAIL, 2 could not run.
#>
param(
  [Parameter(Mandatory)] [string]$App,
  [Parameter(Mandatory)] [string]$Door,
  # A genuine Claude Code claude.exe, kept where SlopBar does not look.
  [Parameter(Mandatory)] [string]$ClaudeExe,
  # The commit the app must be built from (a build of this checkout), or the version it must report.
  [string]$Commit,
  [string]$Version,
  [string]$Work = (Join-Path ([IO.Path]::GetTempPath()) 'slopbar-cli-discovery')
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') {
  Write-Host 'CANNOT RUN: this changes the user PATH in the registry and quits SlopBar, so it runs on a CI runner only.'
  exit 2
}
$App = (Resolve-Path -LiteralPath $App).Path
$Door = (Resolve-Path -LiteralPath $Door).Path
$ClaudeExe = (Resolve-Path -LiteralPath $ClaudeExe).Path
$install = 'irm https://claude.ai/install.ps1 | iex'
$failures = [Collections.Generic.List[string]]::new()
function Check([bool]$ok, [string]$what) {
  if ($ok) { Write-Host "ok    $what" } else { Write-Host "FAIL  $what"; $failures.Add($what) }
}
function Stop-Run([string]$why) { Write-Host "CANNOT RUN: $why"; exit 2 }

# Every place SlopBar looks for a CLI, as it looks: PATH (this process's and the registry's), the
# installers' folders, and the Claude desktop app's folders.
function Get-CliAnywhere {
  $dirs = @($env:Path -split ';') + @([Environment]::GetEnvironmentVariable('Path', 'Machine') -split ';') +
    @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';') +
    @((Join-Path $HOME '.local\bin'), (Join-Path $env:LOCALAPPDATA 'Programs\OpenAI\Codex\bin'), (Join-Path $env:APPDATA 'npm'), (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Links'))
  $hits = @(foreach ($d in $dirs | Where-Object { $_ } | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_.Trim().Trim('"')) }) {
    foreach ($name in 'claude', 'codex') {
      foreach ($ext in '.exe', '.cmd', '.bat', '.com') {
        $f = Join-Path $d "$name$ext"
        if (Test-Path -LiteralPath $f -PathType Leaf) { $f }
      }
    }
  })
  foreach ($data in (Join-Path $env:APPDATA 'Claude\claude-code'), (Join-Path $env:LOCALAPPDATA 'Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude\claude-code')) {
    if (Test-Path -LiteralPath $data) { $hits += $data }
  }
  $hits
}

function Call([string]$tool, [string[]]$arguments = @()) {
  $out = & $Door --call $tool @arguments 2>&1 | Select-Object -Last 1
  try { "$out" | ConvertFrom-Json } catch { [pscustomobject]@{ error = "unreadable answer: $out" } }
}
# add_account, again while the previous sign-in is still being wound up (one per provider).
function Add-Claude {
  $deadline = (Get-Date).AddSeconds(45)
  do {
    $r = Call add_account @('provider=claude')
    if (-not ("$($r.error)" -match 'in progress')) { return $r }
    Start-Sleep 2
  } while ((Get-Date) -lt $deadline)
  $r
}
function Wait-For([scriptblock]$test, [int]$seconds) {
  $deadline = (Get-Date).AddSeconds($seconds)
  do {
    $got = & $test
    if ($got) { return $got }
    Start-Sleep -Milliseconds 500
  } while ((Get-Date) -lt $deadline)
}
# Everything SlopBar started, and what that started (never by image name: other programs are
# claude.exe and cmd.exe too).
function Get-Started([int]$appId) {
  $all = @(Get-CimInstance Win32_Process)
  $tree = [Collections.Generic.List[object]]::new()
  $next = @($appId)
  while ($next) {
    $children = @($all | Where-Object { $next -contains $_.ParentProcessId })
    $children | ForEach-Object { $tree.Add($_) }
    $next = @($children | ForEach-Object { [int]$_.ProcessId })
  }
  $tree
}
# Ends SlopBar's sign-ins: the cmd.exe it started for each (a sign-in's console, a logout) and
# what runs under it. Its own WebView2 processes are left alone.
function Stop-Started([int]$appId) {
  $consoles = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$appId" | Where-Object Name -eq 'cmd.exe')
  foreach ($console in $consoles) {
    Get-Started $console.ProcessId | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Stop-Process -Id $console.ProcessId -Force -ErrorAction SilentlyContinue
  }
}
function Get-SignInFolders { @(Get-ChildItem -LiteralPath $HOME -Directory -Force -Filter '.slopbar-login-*' -ErrorAction SilentlyContinue) }

$before = @(Get-CliAnywhere)
if ($before) { Stop-Run "a CLI is already where SlopBar looks: $($before -join ', ')" }
# wsl.exe lists distros (in UTF-16) with exit code 0, and fails when there are none.
$distros = ''
if (Get-Command wsl.exe -ErrorAction SilentlyContinue) {
  $listed = "$(& wsl.exe --list --quiet 2>$null)" -replace "`0", ''
  if ($LASTEXITCODE -eq 0) { $distros = $listed.Trim() }
}
if ($distros) { Stop-Run "a WSL distro is installed, where a CLI could be: $distros" }
New-Item -ItemType Directory -Force $Work | Out-Null
$userKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
$userPath = $userKey.GetValue('Path', $null, 'DoNotExpandEnvironmentNames')
$userKind = if ($null -ne $userPath) { $userKey.GetValueKind('Path') } else { $null }
$copyRoot = Join-Path $env:APPDATA 'Claude\claude-code'
$appId = $null
try {
  if (-not (Get-Process slopbar -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $App })) {
    $elevated = [bool](& whoami.exe /groups 2>$null | Select-String -SimpleMatch 'S-1-16-12288')
    if ($elevated -and (Get-Process explorer -ErrorAction SilentlyContinue)) {
      Start-Process -FilePath (Join-Path $env:WINDIR 'explorer.exe') -ArgumentList ('"' + $App + '"')
    } else {
      Start-Process -FilePath $App
    }
  }
  $info = Wait-For { (Call get_app_info).ok | Where-Object { $_.version } } 120
  if (-not $info) { Stop-Run 'SlopBar did not answer through its door.' }
  $appId = [int]$info.pid
  $process = Get-CimInstance Win32_Process -Filter "ProcessId=$appId"
  Write-Host "SlopBar $($info.version), commit $($info.build.commit), dirty $($info.build.dirty), process $appId in session $($process.SessionId): $($process.ExecutablePath)"
  if ($Commit -and ($info.build.commit -ne $Commit -or $info.build.dirty)) { Stop-Run "it is built from $($info.build.commit) (dirty $($info.build.dirty)), not $Commit" }
  if ($Version -and $info.version -ne $Version) { Stop-Run "it reports $($info.version), not $Version" }
  $ready = (Call get_machine_readiness).ok
  Write-Host "desktop $($ready.desktop | ConvertTo-Json -Compress), browser $($ready.browser), problems $($ready.problems -join ',')"
  if ($ready.problems | Where-Object { $_ -in 'desktop_locked', 'desktop_not_shown', 'no_desktop_session' }) { Stop-Run 'nobody could see a sign-in on this desktop, so SlopBar starts none' }

  # S0 nothing
  $r = Call add_account @('provider=claude')
  Check ("$($r.error)".Contains($install)) "S0 nothing: add_account fails and names the line that installs Claude Code: $($r.error)"
  Check ("$($r.code)" -eq 'CLI not found') "S0 nothing: the code another computer recognizes goes beside the words: $($r.code)"
  Check ((Get-SignInFolders).Count -eq 0 -and -not (Get-Started $appId | Where-Object Name -eq 'claude.exe')) 'S0 nothing: no sign-in started'

  # S1 unverified
  $version = ("$(& $ClaudeExe --version)" -split ' ')[0]
  $sha = (Get-FileHash -LiteralPath $ClaudeExe -Algorithm SHA256).Hash.ToLowerInvariant()
  $copyDir = Join-Path $copyRoot "$version\$($sha.Substring(0, 12))"
  New-Item -ItemType Directory -Force $copyDir | Out-Null
  $copy = Join-Path $copyDir 'claude.exe'
  Copy-Item -LiteralPath $ClaudeExe $copy
  [IO.File]::WriteAllText((Join-Path $copyDir '.payload'), "{`n  `"sha256`": `"$sha`",`n  `"size`": $((Get-Item -LiteralPath $copy).Length)`n}")
  $r = Call add_account @('provider=claude')
  Check ("$($r.error)".Contains($install)) "S1 unverified: a copy the desktop app has not marked verified is not taken: $($r.error)"
  Check (-not (Get-Started $appId | Where-Object Name -eq 'claude.exe')) 'S1 unverified: nothing started'

  # S2 desktop
  [IO.File]::WriteAllText((Join-Path $copyDir '.verified'), $sha)
  $r = Add-Claude
  Check ($null -ne $r.ok) "S2 desktop: add_account starts a sign-in: $($r | ConvertTo-Json -Compress)"
  $signin = Wait-For { Get-Started $appId | Where-Object { $_.Name -eq 'claude.exe' -and $_.ExecutablePath -eq $copy -and $_.CommandLine -match 'auth\s+login' } | Select-Object -First 1 } 60
  Check ($null -ne $signin) "S2 desktop: claude auth login runs from the desktop app's copy, $copy"
  if ($signin) {
    $console = Get-CimInstance Win32_Process -Filter "ProcessId=$($signin.ParentProcessId)"
    Write-Host "      $($signin.CommandLine)`n      under $($console.CommandLine)"
    Check ($console.Name -eq 'cmd.exe' -and $console.CommandLine -match 'login\.cmd' -and $console.ParentProcessId -eq $appId) 'S2 desktop: in the sign-in console (cmd running login.cmd) SlopBar started'
    Start-Sleep 15
    Check ($null -ne (Get-Process -Id $signin.ProcessId -ErrorAction SilentlyContinue)) 'S2 desktop: the sign-in still waits 15 s later'
  }
  Stop-Started $appId
  # SlopBar winds the ended sign-in up (its logout, then its folder) before the next scene changes
  # what it would find.
  $wound = Wait-For { (Get-SignInFolders).Count -eq 0 } 60
  Check ([bool]$wound) 'S2 desktop: the ended sign-in is wound up (its folder removed)'

  # S3 new PATH
  $newDir = Join-Path $Work 'installed-later'
  $log = Join-Path $Work 'installed-later.log'
  New-Item -ItemType Directory -Force $newDir | Out-Null
  [IO.File]::WriteAllText((Join-Path $newDir 'claude.cmd'), "@echo off`r`n>> `"$log`" echo %~f0 %*`r`nping -n 120 127.0.0.1 >nul`r`n")
  $userKey.SetValue('Path', (@("$userPath".TrimEnd(';'), $newDir) | Where-Object { $_ }) -join ';', [Microsoft.Win32.RegistryValueKind]::ExpandString)
  $r = Add-Claude
  Check ($null -ne $r.ok) "S3 new PATH: add_account starts a sign-in: $($r | ConvertTo-Json -Compress)"
  # The folder's own name (cmd may log the long spelling of an 8.3 TEMP).
  $fake = [regex]::Escape('\installed-later\claude.cmd ')
  $lines = { @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue) }
  $ran = Wait-For { & $lines | Where-Object { $_ -match "$fake.*auth login" } } 60
  Check ($null -ne $ran) "S3 new PATH: the claude.cmd added to the user PATH after SlopBar started runs the sign-in: $((& $lines) -join ' | ')"
  Stop-Started $appId
  # Its first line is the sign-in; SlopBar's logout of the ended sign-in comes after it.
  $out = Wait-For { $all = & $lines; if ($all.Count -ge 2 -and $all[0] -match 'auth login' -and ($all | Select-Object -Skip 1) -match "$fake.*auth logout") { $all } } 40
  Check ($null -ne $out) "S3 new PATH: SlopBar's logout of the ended sign-in runs the same CLI: $((& $lines) -join ' | ')"

  # S4 installer's folder: Anthropic's installer may leave %USERPROFILE%\.local\bin off PATH (it
  # says so and stops there), as it did on a work laptop.
  if ($null -ne $userPath) { $userKey.SetValue('Path', $userPath, $userKind) } else { $userKey.DeleteValue('Path', $false) }
  Check ([bool](Wait-For { (Get-SignInFolders).Count -eq 0 } 60)) 'S3 new PATH: the ended sign-in is wound up'
  $localBin = Join-Path $HOME '.local\bin'
  $onPath = @($env:Path, [Environment]::GetEnvironmentVariable('Path', 'Machine'), [Environment]::GetEnvironmentVariable('Path', 'User')) -split ';' |
    Where-Object { $_ -and [Environment]::ExpandEnvironmentVariables($_.Trim().Trim('"')).TrimEnd('\') -eq $localBin }
  if ($onPath) { Stop-Run "$localBin is on PATH, so S4 would not show it is found off PATH" }
  New-Item -ItemType Directory -Force $localBin | Out-Null
  $installed = Join-Path $localBin 'claude.exe'
  Copy-Item -LiteralPath $ClaudeExe $installed
  $r = Add-Claude
  Check ($null -ne $r.ok) "S4 installer's folder: add_account starts a sign-in: $($r | ConvertTo-Json -Compress)"
  $signin = Wait-For { Get-Started $appId | Where-Object { $_.Name -eq 'claude.exe' -and $_.ExecutablePath -eq $installed -and $_.CommandLine -match 'auth\s+login' } | Select-Object -First 1 } 60
  Check ($null -ne $signin) "S4 installer's folder: claude auth login runs from $installed, which no PATH names (before the desktop app's copy)"
  Stop-Started $appId
  Check ([bool](Wait-For { (Get-SignInFolders).Count -eq 0 } 60)) "S4 installer's folder: the ended sign-in is wound up"
} finally {
  if ($null -ne $userPath) { $userKey.SetValue('Path', $userPath, $userKind) } else { $userKey.DeleteValue('Path', $false) }
  $userKey.Close()
  if ($appId) {
    Call quit_app | Out-Null
    Stop-Started $appId
    $quit = Wait-For { -not (Get-Process -Id $appId -ErrorAction SilentlyContinue) } 20
    if (-not $quit) { Stop-Process -Id $appId -Force -ErrorAction SilentlyContinue }
  }
}
Check ("$([Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment').GetValue('Path', $null, 'DoNotExpandEnvironmentNames'))" -eq "$userPath") 'the user PATH is as it was'

if ($failures.Count) { Write-Host "FAIL ($($failures.Count))"; exit 1 }
Write-Host 'PASS'
exit 0
