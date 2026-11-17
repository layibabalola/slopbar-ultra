<#
Installs SlopBar Ultra on this Windows computer, for the user signed in, from the fork's release
channel. It is for a computer the fleet can't reach over SSH, such as a work laptop. A computer the
fleet can reach is set up from another machine instead (docs/ULTRA-RELEASES.md).

Run this line in a normal PowerShell window, not one run as administrator. Windows PowerShell 5.1
and PowerShell 7 both work, and it needs no admin rights:

  irm https://raw.githubusercontent.com/layibabalola/slopbar-ultra/ultra/scripts/install-ultra.ps1 | iex

What it does:
1. Reads the update channel (latest.json in the ultra-latest release) to find the newest version.
2. If SlopBar Ultra is already installed here, it runs no installer. It asks the app, through its
   door (slopbar-mcp.exe --call), which version is running. If the channel has a newer release, it
   asks the app to install it (install_update). That install waits for sign-ins, as every update
   does. If SlopBar that is not an Ultra release is installed here, it changes nothing and says so.
3. Otherwise it downloads that version's installer and checks its size and SHA-256 against the
   values GitHub's API gives for the release asset. Then it runs the installer quietly (/S, for this
   user only, no admin), starts SlopBar Ultra and waits for the door to answer. It checks the
   version, and checks slopbar-mcp.exe's SHA-256 against the release notes.
4. It turns on Update automatically and Launch at login, and reports how both now stand.

The last line it prints is "RESULT: OK - ..." or "RESULT: FAILED at <step>: <reason>", and
$global:SlopBarUltraInstall holds the same outcome. It never exits the shell, so the window stays
open under `| iex`. You sign in to your accounts yourself, in SlopBar Ultra.

If a proxy, AppLocker, WDAC, Smart App Control or antivirus blocks the download or the installer,
the script stops at that step and says so. It never tries to work around a block.

To download and check the installer without installing or changing anything, first set
$env:SLOPBAR_ULTRA_CHECK_ONLY = '1'.

Keep this file ASCII and free of `exit`: it runs through Invoke-Expression in the user's own shell,
and in Constrained Language mode where a policy enforces it, so use core types only.
#>

function Install-SlopBarUltra {
  param([switch]$CheckOnly)

  $ErrorActionPreference = 'Stop'
  # Windows PowerShell 5.1 downloads very slowly while it draws a progress bar.
  $ProgressPreference = 'SilentlyContinue'
  $repo = 'layibabalola/slopbar-ultra'
  $agent = 'slopbar-ultra-install'
  # {"tool":"update_settings","arguments":{"patch":{"launch_at_login":true,"auto_update":true}}} in
  # base64url. Windows PowerShell 5.1 drops the quotes inside a JSON argument it passes to a program,
  # so the door gets this payload encoded instead.
  $turnOn = 'eyJ0b29sIjoidXBkYXRlX3NldHRpbmdzIiwiYXJndW1lbnRzIjp7InBhdGNoIjp7ImxhdW5jaF9hdF9sb2dpbiI6dHJ1ZSwiYXV0b191cGRhdGUiOnRydWV9fX0'
  $step = 'start'
  $version = $null
  $work = $null
  $door = $null

  function Say([string]$Text) { Write-Host "  $Text" }

  function Write-Outcome([bool]$Ok, [string]$Text) {
    if ($Ok) { Write-Host "RESULT: OK - $Text" -ForegroundColor Green }
    else { Write-Host "RESULT: FAILED at $($step): $Text" -ForegroundColor Red }
    # A hashtable, not [pscustomobject]: Constrained Language mode refuses that cast in 5.1.
    $global:SlopBarUltraInstall = @{ ok = $Ok; step = $step; message = $Text; version = $version; computer = $env:COMPUTERNAME }
  }

  # Runs one tool through the door. Returns @{ ok; error } from SlopBar's answer, @{ blocked } when
  # slopbar-mcp.exe could not run at all, or $null when nothing answered.
  function Invoke-Door([string[]]$CallArgs) {
    # Under 'Stop', Windows PowerShell 5.1 turns a program's stderr line into a terminating error.
    $ErrorActionPreference = 'Continue'
    if (-not $door -or -not (Test-Path -LiteralPath $door)) { return $null }
    try { $line = & $door @CallArgs 2>$null | Select-Object -Last 1 }
    catch { return @{ blocked = $_.Exception.Message } }
    if (-not $line) { return $null }
    try { $answer = $line | ConvertFrom-Json } catch { return $null }
    return @{ ok = $answer.ok; error = $answer.error }
  }

  # Calls get_app_info until the door answers with a version, or until the seconds run out. Throws at
  # once when slopbar-mcp.exe cannot run at all, since waiting will not change that.
  function Wait-Door([int]$Seconds) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ($true) {
      $r = Invoke-Door @('--call', 'get_app_info')
      if ($r -and $r.blocked) { throw "slopbar-mcp.exe could not run: $($r.blocked)" }
      if ($r -and $r.ok -and $r.ok.version) { return $r.ok }
      if ((Get-Date) -ge $deadline) { return $null }
      Start-Sleep -Seconds 2
    }
  }

  # Where SlopBar is installed for this user: the uninstall entry's folder when it holds slopbar.exe,
  # else the installer's default.
  function Resolve-InstallDir {
    $found = Join-Path $env:LOCALAPPDATA 'SlopBar'
    try {
      $location = ([string](Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\SlopBar' -ErrorAction Stop).InstallLocation).Trim('"')
      if ($location -and (Test-Path -LiteralPath (Join-Path $location 'slopbar.exe'))) { $found = $location }
    } catch { }
    return $found
  }

  # Starts SlopBar Ultra unless this copy already runs. From an elevated window it starts it through
  # Explorer, as the user normally runs it. An elevated app's door admits only elevated clients, so
  # the user's agents could not reach it until the next sign-in.
  function Start-App {
    if (Get-Process -Name slopbar -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $app }) { return }
    if ($elevated -and $desktop.Count -gt 0) { Start-Process -FilePath (Join-Path $env:WINDIR 'explorer.exe') -ArgumentList ('"' + $app + '"') }
    else { Start-Process -FilePath $app }
  }

  # Whether Windows has a startup entry (HKCU Run) that starts this copy of SlopBar Ultra. The entry
  # holds the path the app was started from, which can be the long or the short (8.3) form of this
  # one, so its program is compared as a file (size and time written), not as text.
  function Test-StartupEntry {
    try { $run = Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -ErrorAction Stop } catch { return $false }
    $mine = Get-Item -LiteralPath $app -ErrorAction SilentlyContinue
    if (-not $mine) { return $false }
    foreach ($property in $run.PSObject.Properties) {
      if ([string]$property.Value -notmatch '^\s*"?([^"]*?slopbar\.exe)') { continue }
      $theirs = Get-Item -LiteralPath $Matches[1] -ErrorAction SilentlyContinue
      if ($theirs -and $theirs.Length -eq $mine.Length -and $theirs.LastWriteTimeUtc -eq $mine.LastWriteTimeUtc) { return $true }
    }
    return $false
  }

  # Turns on Update automatically and Launch at login, and returns how both now stand. Launch at
  # login counts as on only when Windows also has the startup entry, because the app ignores a
  # failure to write it.
  function Set-Defaults {
    $r = Invoke-Door @('--call-b64', $turnOn)
    $saved = if ($r) { $r.ok } else { $null }
    $updates = if ($saved -and $saved.auto_update -eq $true) { 'on' } else { 'not on (turn it on in Settings, Updates)' }
    $entry = Test-StartupEntry
    if (-not $entry) { Start-Sleep -Seconds 2; $entry = Test-StartupEntry }
    $atLogin = 'not on (turn it on in Settings)'
    if ($saved -and $saved.launch_at_login -eq $true) {
      $atLogin = if ($entry) { 'on' } else { 'set, but Windows has no startup entry for it (a policy may block startup apps)' }
    }
    return "Update automatically: $updates. Launch at login: $atLogin."
  }

  # The SHA-256 of a file. In Windows PowerShell 5.1 Get-FileHash is script code. If a policy runs
  # it in Constrained Language mode, it can't create its hasher, so certutil computes the digest
  # instead.
  function Get-Sha256([string]$Path) {
    try { return [string](Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash } catch { }
    $ErrorActionPreference = 'Continue'
    $out = & certutil.exe -hashfile $Path SHA256 2>$null
    $line = $out | Where-Object { $_ -match '^\s*[0-9a-fA-F]{2}( ?[0-9a-fA-F]{2}){31}\s*$' } | Select-Object -First 1
    if ($line) { return ($line -replace '\s', '') }
    throw 'neither Get-FileHash nor certutil could compute the SHA-256 of the download'
  }

  # Whether the WebView2 Runtime, which SlopBar Ultra draws its windows with, is installed.
  function Test-WebView2 {
    $client = 'Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    foreach ($key in "HKLM:\SOFTWARE\WOW6432Node\$client", "HKLM:\SOFTWARE\$client", "HKCU:\Software\$client") {
      try { $pv = [string](Get-ItemProperty -Path $key -ErrorAction Stop).pv } catch { $pv = '' }
      if ($pv -and $pv -ne '0.0.0.0') { return $true }
    }
    return $false
  }

  # A sortable key for an <x.y.z>-ultra.<n> version. Returns $null for anything else, such as a
  # local build or upstream SlopBar.
  function Get-UltraKey([string]$Version) {
    if ($Version -match '^(\d+)\.(\d+)\.(\d+)-ultra\.(\d+)$') {
      return '{0:D6}.{1:D6}.{2:D6}.{3:D9}' -f [int]$Matches[1], [int]$Matches[2], [int]$Matches[3], [int]$Matches[4]
    }
    return $null
  }

  # Adds a plain reason to the errors that a network or a computer's policy causes.
  function Get-Reason([string]$Message) {
    $hint = $null
    if ($Message -match 'blocked by group policy|Application Control|App Control|Device Guard|administrator has blocked|not allowed by policy|cannot verify the digital signature') {
      $hint = "This computer's policy blocked it. Ask its administrator; this script does not work around a block."
    } elseif ($Message -match 'virus|potentially unwanted') {
      $hint = 'Antivirus stopped it.'
    } elseif ($Message -match 'rate limit') {
      $hint = "GitHub's hourly limit for this network's address is used up. Run the line again later."
    } elseif ($Message -match 'JSON primitive|Conversion from JSON failed|without JSON') {
      $hint = "Something on the network answered in GitHub's place, probably a proxy's block page."
    } elseif ($Message -match '\(407\)') {
      $hint = "The network's proxy wants a sign-in that PowerShell could not give it."
    } elseif ($Message -match 'trust relationship|SSL|TLS|could not be resolved|Unable to connect|timed out|connection was closed|proxy|\((403|502|503)\)') {
      $hint = 'The network blocked or intercepted the connection to GitHub.'
    } elseif ($step -match 'installer|start' -and $Message -match 'Access is denied') {
      $hint = 'Security software or a policy on this computer refused to run it.'
    }
    if ($hint) { return "$Message ($hint)" }
    return $Message
  }

  try {
    # On an older .NET, Windows PowerShell 5.1 may not offer TLS 1.2, and GitHub requires it. The
    # next two lines let PowerShell sign in to a proxy as the user, the way the browser does.
    # Constrained Language mode refuses all three; the defaults apply there.
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }
    try { [Net.WebRequest]::DefaultWebProxy.Credentials = [Net.CredentialCache]::DefaultNetworkCredentials } catch { }
    try { [Net.Http.HttpClient]::DefaultProxy.Credentials = [Net.CredentialCache]::DefaultNetworkCredentials } catch { }

    $step = 'check this window'
    # SlopBar Ultra installs for the user who runs the line. A window started as another user (an
    # administrator's account, say) would install it for that user, not for the one at the desktop.
    $desktop = @()
    try {
      $desktop = @(Get-CimInstance -ClassName Win32_Process -Filter "Name='explorer.exe'" -ErrorAction Stop |
        ForEach-Object { [string](Invoke-CimMethod -InputObject $_ -MethodName GetOwner -ErrorAction Stop).User } |
        Where-Object { $_ })
    } catch { }
    if ($desktop.Count -gt 0 -and -not ($desktop -contains $env:USERNAME)) {
      throw "this window runs as $env:USERNAME, but the desktop belongs to $($desktop -join ', '). Run the line in a normal PowerShell window."
    }
    $elevated = $false
    try { $elevated = [bool](& whoami.exe /groups 2>$null | Select-String -SimpleMatch 'S-1-16-12288') } catch { }
    if ($elevated) { Say 'This window runs as administrator. SlopBar Ultra is still started through Explorer, as your normal user.' }

    $step = 'find the install folder'
    $dir = Resolve-InstallDir
    $app = Join-Path $dir 'slopbar.exe'
    $door = Join-Path $dir 'slopbar-mcp.exe'

    $step = 'read the update channel'
    $work = Join-Path $env:TEMP ('slopbar-ultra-install-' + (Get-Random))
    New-Item -ItemType Directory -Path $work -Force | Out-Null
    $manifest = Join-Path $work 'latest.json'
    Invoke-WebRequest -Uri "https://github.com/$repo/releases/download/ultra-latest/latest.json" -OutFile $manifest -UseBasicParsing -UserAgent $agent -TimeoutSec 60
    $latest = Get-Content -Raw -LiteralPath $manifest | ConvertFrom-Json
    $version = [string]$latest.version
    if (-not (Get-UltraKey $version)) { throw "the channel names version '$version', which is not <x.y.z>-ultra.<n>" }
    $name = "SlopBar_$($version)_x64-setup.exe"
    $url = "https://github.com/$repo/releases/download/ultra-v$version/$name"
    $named = [string]$latest.platforms.'windows-x86_64'.url
    if ($named -ne $url) { throw "the channel's installer is '$named', not $url" }
    Say "The channel has SlopBar Ultra $version."

    if (Test-Path -LiteralPath $app) {
      $step = 'ask the installed SlopBar its version'
      if (-not (Test-Path -LiteralPath $door)) {
        throw "SlopBar in $dir has no slopbar-mcp.exe, so it is upstream SlopBar or a damaged install. Nothing was changed. Uninstall it (Settings, Apps), then run the line again."
      }
      $info = Wait-Door 5
      if (-not $info) {
        Say 'SlopBar Ultra is installed here but did not answer. Starting it.'
        Start-App
        $info = Wait-Door 90
      }
      if (-not $info) { throw "SlopBar is installed in $dir, but it did not answer through slopbar-mcp.exe within 90 seconds" }
      $have = [string]$info.version
      $haveKey = Get-UltraKey $have
      if (-not $haveKey) {
        throw "SlopBar $have is installed here, and it is not an Ultra release, so nothing was changed. To move it to the Ultra channel, run the installer from the newest ultra-v release over it, then turn on Update automatically in Settings."
      }
      if ($haveKey -ge (Get-UltraKey $version)) {
        if ($CheckOnly) {
          Write-Outcome $true "check only: SlopBar Ultra $have is installed and current. Nothing was changed."
          return
        }
        $step = 'turn on updates and Launch at login'
        $state = Set-Defaults
        Write-Outcome $true "SlopBar Ultra $have is already installed and running on $env:COMPUTERNAME. $state"
        return
      }
      if ($CheckOnly) {
        Write-Outcome $true "check only: SlopBar Ultra $have runs here, and the channel has $version. Nothing was changed."
        return
      }
      $step = 'update the installed SlopBar Ultra'
      Say "SlopBar Ultra $have runs here. Asking it to install $version."
      $target = Get-UltraKey $version
      $deadline = (Get-Date).AddMinutes(15)
      $askedAt = (Get-Date).AddMinutes(-10)
      $refusals = 0
      while ($true) {
        $r = Invoke-Door @('--call', 'get_app_info')
        if ($r -and $r.blocked) { throw "slopbar-mcp.exe could not run: $($r.blocked)" }
        $info = if ($r) { $r.ok } else { $null }
        $nowKey = if ($info) { Get-UltraKey ([string]$info.version) } else { $null }
        if ($nowKey -and $nowKey -ge $target) { break }
        if ((Get-Date) -ge $deadline) {
          $seen = if ($info) { [string]$info.version } else { 'no answer' }
          $why = ''
          if ($info -and $info.update_check) { $why = (" Its last update check: $($info.update_check.result) $($info.update_check.reason) $($info.update_check.error)").TrimEnd() + '.' }
          throw "SlopBar Ultra was asked to install $version, but after 15 minutes it reports $seen.$why"
        }
        # The app can defer a request (while a sign-in is open, or another install runs), and
        # nothing retries it. So ask again every 90 seconds, as the fleet report does.
        if ($info -and ((Get-Date) - $askedAt).TotalSeconds -ge 90) {
          $r = Invoke-Door @('--call', 'install_update')
          if ($r -and $r.error) {
            $refusals++
            if ($refusals -ge 3) { throw "SlopBar Ultra refused the update three times: $($r.error)" }
          } else { $refusals = 0 }
          $askedAt = Get-Date
        }
        Start-Sleep -Seconds 10
      }
      $version = [string]$info.version
      $step = 'turn on updates and Launch at login'
      $state = Set-Defaults
      Write-Outcome $true "SlopBar Ultra $version is installed and running on $env:COMPUTERNAME. $state"
      return
    }

    $step = 'ask GitHub for the installer digest'
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/tags/ultra-v$version" -UseBasicParsing -UserAgent $agent -TimeoutSec 60 -Headers @{ Accept = 'application/vnd.github+json' }
    if ($release -is [string] -or -not $release.assets) { throw 'api.github.com answered without JSON' }
    $asset = @($release.assets | Where-Object { $_.name -eq $name })[0]
    if (-not $asset) { throw "release ultra-v$version has no $name" }
    if ([string]$asset.digest -match '^sha256:([0-9a-f]{64})$') { $sha = $Matches[1] }
    else { throw "GitHub gives no SHA-256 digest for $name" }
    $mcpSha = $null
    if ([string]$release.body -match 'slopbar-mcp\.exe SHA-256: ([0-9a-f]{64})') { $mcpSha = $Matches[1] }

    $step = 'download the installer'
    $setup = Join-Path $work $name
    Invoke-WebRequest -Uri $url -OutFile $setup -UseBasicParsing -UserAgent $agent -TimeoutSec 900
    $size = (Get-Item -LiteralPath $setup).Length
    if ($size -ne [long]$asset.size) { throw "the download has $size bytes, but the release asset has $($asset.size). A proxy may have blocked or changed it." }
    $got = Get-Sha256 $setup
    if ($got -ne $sha) { throw "its SHA-256 is $got, not GitHub's $sha" }
    Say ('Downloaded {0} ({1:N1} MB). Its SHA-256 matches the digest GitHub gives.' -f $name, ($size / 1MB))

    if ($CheckOnly) {
      Write-Outcome $true "check only: the installer for $version downloads and checks out. Nothing was installed."
      return
    }

    $step = 'run the installer'
    $hasWebView2 = Test-WebView2
    if (-not $hasWebView2) { Say 'The WebView2 Runtime is missing. The installer fetches it from Microsoft (go.microsoft.com).' }
    Say 'Running the installer quietly, for this user only.'
    # Wait for the installer alone. -Wait would also wait for every process it starts, and the
    # WebView2 bootstrapper can leave an updater running.
    $proc = Start-Process -FilePath $setup -ArgumentList '/S' -PassThru
    $proc | Wait-Process -Timeout 900
    if ($null -ne $proc.ExitCode -and $proc.ExitCode -ne 0) {
      if ($proc.ExitCode -eq 2 -and -not $hasWebView2) {
        throw 'the installer stopped before installing anything (code 2), because the WebView2 Runtime it needs could not be downloaded or installed. Install the WebView2 Runtime from Microsoft, then run the line again.'
      }
      throw "the installer exited with code $($proc.ExitCode)"
    }
    # The installer reuses a folder an earlier install chose, so look again.
    $dir = Resolve-InstallDir
    $app = Join-Path $dir 'slopbar.exe'
    $door = Join-Path $dir 'slopbar-mcp.exe'
    if (-not (Test-Path -LiteralPath $app) -or -not (Test-Path -LiteralPath $door)) { throw "the installer finished, but $app or $door is missing" }

    $step = 'start SlopBar Ultra'
    Start-App
    $info = Wait-Door 120
    if (-not $info) { throw 'SlopBar Ultra did not answer through slopbar-mcp.exe within 120 seconds' }

    $step = 'check what runs'
    if ([string]$info.version -ne $version) { throw "it reports $($info.version), not $version" }
    if ($mcpSha -and [string]$info.mcp.sha256 -ne $mcpSha) { throw "slopbar-mcp.exe's SHA-256 is $($info.mcp.sha256), not $mcpSha from the release notes" }
    if (-not $mcpSha) { Say 'The release notes give no SHA-256 for slopbar-mcp.exe, so that check was skipped.' }

    $step = 'turn on updates and Launch at login'
    $state = Set-Defaults
    Say 'Next: open SlopBar Ultra from the tray and sign in to your accounts.'
    Write-Outcome $true "SlopBar Ultra $version is installed and running on $env:COMPUTERNAME. $state"
  } catch {
    $message = $_.Exception.Message
    # Windows PowerShell 5.1 keeps GitHub's own words (such as its rate limit) out of the message.
    if ([string]$_.ErrorDetails.Message -match 'rate limit') { $message = "$message (GitHub: API rate limit exceeded)" }
    Write-Outcome $false (Get-Reason $message)
  } finally {
    # Some TEMP folders (C:\Windows\Temp, for one) let a user create a folder but not delete it,
    # and Remove-Item then throws even with -ErrorAction SilentlyContinue. A leftover never changes
    # the outcome.
    if ($work) { try { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction Stop } catch { } }
  }
}

Install-SlopBarUltra -CheckOnly:($env:SLOPBAR_ULTRA_CHECK_ONLY -eq '1')
