#Requires -Version 7
<#
Reports every fleet machine's SlopBar Ultra against the update channel (latest.json of the
ultra-latest release). With -UpdateNow, every machine that is behind installs the channel's release
now, and the script waits until each one reports it. See docs/ULTRA-RELEASES.md ("The fleet").

  pwsh scripts/fleet-slopbar.ps1              # report
  pwsh scripts/fleet-slopbar.ps1 -UpdateNow   # and update the machines that are behind

Machines: this one, plus each stanza with a HostName in the managed block of ~/.ssh/config
(between "# >>> fleet-ssh managed block" and "# <<< fleet-ssh managed block <<<"), so no host name
or address lives in this repository. Each machine answers through its own slopbar-mcp.exe
(--call get_app_info, get_settings, install_update). Over ssh the command is `cmd /d /c ...`, which
works whether the remote shell is PowerShell or cmd, as long as the profile path has no spaces.
ssh runs through Start-Process with its output in files, a timeout and a tree kill: on some
machines an ssh.exe whose output is piped never exits.

LastCheck is each machine's last update check (get_app_info's update_check): how long ago it ran,
and what it found, why it was deferred, or its error, cut to 40 characters; the table is followed
by each one that was cut, in full. A build from before that record shows n/a, which is no problem,
because machines update one by one. The limits below come from each machine's own record: its
update interval and the limit of one pass of its update loop.

Problems (exit code 1): a local build (a version without -ultra., which takes no releases), a build
without updates, "Update automatically" off, a machine that does not answer, an update that does not
arrive, and, from the record:
  - a failed last update check (a deferred one is no problem);
  - no update check for longer than the interval plus a pass's limit while "Update automatically" is
    on;
  - an update loop whose next run is overdue by more than a pass's limit: it has stopped, even if a
    check asked for since then shows a recent time;
  - a machine still behind the channel longer than the interval plus a pass's limit after the
    channel's release was published (latest.json's pub_date).
A machine without SlopBar is reported as not installed, which is no problem.

The functions are tested offline by scripts/test-fleet-slopbar.ps1, which dot-sources this file.
#>
param(
  [switch]$UpdateNow,
  [string]$Repo = 'layibabalola/slopbar-ultra',
  [string]$SshConfig = (Join-Path $HOME '.ssh/config'),
  [int]$TimeoutSeconds = 45,
  [int]$InstallWaitSeconds = 300
)
$ErrorActionPreference = 'Stop'
$here = if ($env:COMPUTERNAME) { $env:COMPUTERNAME } else { [Environment]::MachineName }
$door = '%LOCALAPPDATA%\SlopBar\slopbar-mcp.exe'

# The fleet's ssh aliases: the first name of each managed stanza that has a HostName, except this machine.
function Get-FleetHosts([string]$path) {
  if (-not (Test-Path -LiteralPath $path)) { return @() }
  $block = [regex]::Match((Get-Content -LiteralPath $path -Raw), '(?s)# >>> fleet-ssh managed block.*?# <<< fleet-ssh managed block <<<')
  if (-not $block.Success) { return @() }
  $self = $here.ToLowerInvariant() -replace '[^a-z0-9]', ''
  $found = [Collections.Generic.List[string]]::new()
  $aliases = $null
  foreach ($line in $block.Value -split "`r?`n") {
    if ($line -match '^\s*Host\s+(.+?)\s*$') { $aliases = @($Matches[1] -split '\s+') }
    elseif ($aliases -and $line -match '^\s*HostName\s+\S') {
      $isSelf = @($aliases | Where-Object { ($_.ToLowerInvariant() -replace '[^a-z0-9]', '') -eq $self }).Count -gt 0
      if (-not $isSelf -and $aliases[0] -notmatch '[*?!]') { $found.Add($aliases[0]) }
      $aliases = $null
    }
  }
  return $found.ToArray()
}

# Whether Ultra version $a (<x.y.z>-ultra.<n>) is newer than $b.
function Test-NewerVersion([string]$a, [string]$b) {
  $pattern = '^(\d+\.\d+\.\d+)-ultra\.(\d+)$'
  if ($a -notmatch $pattern) { return $false }
  $packageA = [version]$Matches[1]; $buildA = [int]$Matches[2]
  if ($b -notmatch $pattern) { return $false }
  $packageB = [version]$Matches[1]; $buildB = [int]$Matches[2]
  return $packageA -gt $packageB -or ($packageA -eq $packageB -and $buildA -gt $buildB)
}

# A UTC time from get_app_info or latest.json (ConvertFrom-Json may already have made it a
# [datetime]); both always write UTC.
function ConvertTo-UtcTime($value) {
  if ($null -eq $value -or "$value" -eq '') { return $null }
  if ($value -is [datetime]) {
    if ($value.Kind -eq [DateTimeKind]::Unspecified) { return [datetime]::SpecifyKind($value, [DateTimeKind]::Utc) }
    return $value.ToUniversalTime()
  }
  return [datetimeoffset]::Parse("$value", [Globalization.CultureInfo]::InvariantCulture).UtcDateTime
}

# $text cut to $max characters.
function Get-Short([string]$text, [int]$max = 40) {
  if ($text.Length -le $max) { return $text }
  return $text.Substring(0, $max - 1) + '…'
}

# The app's last update check (get_app_info's update_check): its text, the problems it shows, and
# the machine's update interval and pass limit, in seconds. A build from before the record has no
# update_check: n/a, no problem. Problems: a failed check (a deferred one is none); no check for
# longer than the interval plus a pass's limit while "Update automatically" is on; and an update
# loop whose next run is overdue by more than a pass's limit, which means it has stopped, whatever
# a check asked for since then shows.
function Get-LastCheck($check, $auto, [datetime]$now = [datetime]::UtcNow) {
  if ($null -eq $check) { return [pscustomobject]@{ Text = 'n/a'; Problems = @(); Interval = $null; PassLimit = $null } }
  $interval = [double]($check.interval_seconds ?? 900)
  $passLimit = [double]($check.pass_limit_seconds ?? 2140)
  $problems = [Collections.Generic.List[string]]::new()
  $due = ConvertTo-UtcTime $check.next_at
  if ($null -ne $due -and ($now - $due).TotalSeconds -gt $passLimit + 60) {
    $problems.Add("update loop stopped: next check $([int][Math]::Floor(($now - $due).TotalMinutes)) min overdue")
  }
  $at = ConvertTo-UtcTime $check.at
  if ($null -eq $at) {
    return [pscustomobject]@{ Text = 'none yet'; Problems = $problems.ToArray(); Interval = $interval; PassLimit = $passLimit }
  }
  $age = $now - $at
  $minutes = [int][Math]::Max(0, [Math]::Floor($age.TotalMinutes))
  $result = switch ("$($check.result)") {
    'up_to_date' { 'up to date' }
    'found' { "found $($check.version)" }
    'installing' { "installing $($check.version)" }
    'deferred' { "deferred: $($check.reason)" }
    'error' { "error: $($check.error)" }
    default { "$($check.result)" }
  }
  if ("$($check.result)" -eq 'error') { $problems.Add('last update check failed') }
  elseif ($auto -ne $false -and $age.TotalSeconds -gt $interval + $passLimit) { $problems.Add("no update check for $minutes min") }
  return [pscustomobject]@{ Text = "$minutes min ago: $result"; Problems = $problems.ToArray(); Interval = $interval; PassLimit = $passLimit }
}

# Starts one door call on a machine; Complete-Call collects it.
function Start-Call([string]$machine, [string]$tool) {
  $id = [guid]::NewGuid().ToString('N')
  $out = Join-Path $scratch "$id.out"
  $err = Join-Path $scratch "$id.err"
  # No SlopBar: `if exist` prints nothing and exits 0, whatever the system's language.
  $command = "/d /c if exist $door $door --call $tool"
  $p = if ($machine -eq $here) {
    Start-Process -FilePath cmd.exe -ArgumentList $command -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru
  } else {
    Start-Process -FilePath ssh -ArgumentList "-n -o BatchMode=yes -o ConnectTimeout=10 $machine cmd $command" -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru
  }
  $null = $p.Handle  # keeps ExitCode readable after the process ends
  [pscustomobject]@{ Machine = $machine; Tool = $tool; Process = $p; Out = $out; Err = $err }
}

# The call's answer: State ok (Value), missing, error, unreachable, timeout or failed (Message).
function Complete-Call($call, [datetime]$deadline) {
  $left = [int][Math]::Max(0, ($deadline - (Get-Date)).TotalMilliseconds)
  $timedOut = -not $call.Process.WaitForExit($left)
  if ($timedOut) { & taskkill.exe /PID $call.Process.Id /T /F *> $null }
  $text = "$(Get-Content -LiteralPath $call.Out -Raw -ErrorAction SilentlyContinue)".Trim()
  $err = @("$(Get-Content -LiteralPath $call.Err -Raw -ErrorAction SilentlyContinue)" -split "`r?`n" |
      Where-Object { $_.Trim() -and $_ -notmatch '^\*\*' })
  $code = $call.Process.ExitCode
  $line = @($text -split "`r?`n" | Where-Object { $_ -match '^\s*\{' })[0]
  if ($timedOut) { return [pscustomobject]@{ State = 'timeout'; Message = "no answer within $TimeoutSeconds s" } }
  if ($line) {
    $json = $line | ConvertFrom-Json
    if ($json.PSObject.Properties.Name -contains 'ok') { return [pscustomobject]@{ State = 'ok'; Value = $json.ok } }
    return [pscustomobject]@{ State = 'error'; Message = "$($json.error ?? $line)" }
  }
  if (-not $text -and $code -eq 0) { return [pscustomobject]@{ State = 'missing' } }
  if ($code -eq 255) { return [pscustomobject]@{ State = 'unreachable'; Message = ($err | Select-Object -Last 1) } }
  return [pscustomobject]@{ State = 'failed'; Message = "exit $code $(($err | Select-Object -Last 1) ?? $text)".Trim() }
}

# Runs one tool on every given machine at once.
function Invoke-Everywhere([string[]]$machines, [string]$tool, [int]$seconds = $TimeoutSeconds) {
  $calls = @(foreach ($m in $machines) { Start-Call $m $tool })
  $deadline = (Get-Date).AddSeconds($seconds)
  $answers = @{}
  foreach ($c in $calls) { $answers[$c.Machine] = Complete-Call $c $deadline }
  return $answers
}

# One machine's row. $published is when the channel's release was published (latest.json's
# pub_date).
function Get-Row([string]$machine, $info, $settings, [string]$channel, $published, [datetime]$now = [datetime]::UtcNow) {
  $row = [pscustomobject]@{ Machine = $(if ($machine -eq $here) { "$($machine.ToLowerInvariant()) (here)" } else { $machine })
    Version = ''; AutoUpdate = ''; LastCheck = ''; Status = ''; Behind = $false; Problem = $false; Key = $machine; CheckText = '' }
  switch ($info.State) {
    'missing' { $row.Status = 'not installed' }
    'ok' {
      $version = "$($info.Value.version)"
      $row.Version = $version
      $auto = if ($settings.State -eq 'ok') { [bool]$settings.Value.auto_update } else { $null }
      $row.AutoUpdate = if ($null -eq $auto) { '?' } elseif ($auto) { 'on' } else { 'off' }
      $notes = [Collections.Generic.List[string]]::new()
      if ($version -notmatch '-ultra\.') { $notes.Add('local build: takes no releases'); $row.Problem = $true }
      elseif (-not $info.Value.updates_enabled) { $notes.Add('build without updates'); $row.Problem = $true }
      elseif ($version -eq $channel) { $notes.Add('current') }
      elseif (Test-NewerVersion $channel $version) { $notes.Add('behind'); $row.Behind = $true }
      else { $notes.Add('newer than the channel'); $row.Problem = $true }
      if ($auto -eq $false) { $notes.Add('auto-update off'); $row.Problem = $true }
      if ($settings.State -ne 'ok') { $notes.Add("settings unreadable ($($settings.State))"); $row.Problem = $true }
      $last = Get-LastCheck $info.Value.update_check $auto $now
      $row.CheckText = $last.Text
      $row.LastCheck = Get-Short $last.Text
      foreach ($problem in $last.Problems) { $notes.Add($problem); $row.Problem = $true }
      # Still behind long after the release: its update loop should have installed it by now.
      $released = ConvertTo-UtcTime $published
      if ($row.Behind -and $null -ne $last.Interval -and $null -ne $released) {
        $since = $now - $released
        if ($since.TotalSeconds -gt $last.Interval + $last.PassLimit) {
          $notes.Add("still behind $([int][Math]::Floor($since.TotalMinutes)) min after the release"); $row.Problem = $true
        }
      }
      $row.Status = $notes -join '; '
    }
    default { $row.Status = "$($info.State): $($info.Message)"; $row.Problem = $true }
  }
  return $row
}

function Get-Report([string[]]$machines, [string]$channel, $published) {
  $info = Invoke-Everywhere $machines 'get_app_info'
  $settings = Invoke-Everywhere $machines 'get_settings'
  return @(foreach ($m in $machines) { Get-Row $m $info[$m] $settings[$m] $channel $published })
}

# The table, each last check cut to fit, then those that were cut, in full.
function Write-Report($rows) {
  $rows | Format-Table Machine, Version, AutoUpdate, LastCheck, Status -AutoSize | Out-String -Width 200 | Write-Output
  $cut = @($rows | Where-Object { $_.CheckText -ne $_.LastCheck })
  if ($cut) {
    Write-Output 'Last checks in full:'
    foreach ($row in $cut) { Write-Output "  $($row.Key): $($row.CheckText)" }
    Write-Output ''
  }
}

# Dot-sourced (scripts/test-fleet-slopbar.ps1): define the functions only.
if ($MyInvocation.InvocationName -eq '.') { return }

$scratch = Join-Path ([IO.Path]::GetTempPath()) "slopbar-fleet-$PID"
$null = New-Item -ItemType Directory -Force $scratch
try {
  $latest = Invoke-RestMethod "https://github.com/$Repo/releases/download/ultra-latest/latest.json" -TimeoutSec 30
  $channel = "$($latest.version)"
  $published = $latest.pub_date
  if ($channel -notmatch '^\d+\.\d+\.\d+-ultra\.\d+$') { throw "The channel holds '$channel', not an Ultra version." }
  $machines = @($here) + @(Get-FleetHosts $SshConfig)
  Write-Output "Channel: $channel. Machines: $($machines.Count) (this one and the ssh managed block)."
  $rows = Get-Report $machines $channel $published
  Write-Report $rows

  if ($UpdateNow) {
    $behind = @($rows | Where-Object Behind | ForEach-Object Key)
    if (-not $behind) {
      Write-Output 'No machine is behind the channel.'
    } else {
      Write-Output "Installing $channel on: $($behind -join ', ')"
      $replies = Invoke-Everywhere $behind 'install_update'
      foreach ($m in $behind) {
        $r = $replies[$m]
        Write-Output "  ${m}: $(if ($r.State -eq 'ok') { 'installing' } else { "$($r.State) $($r.Message)" })"
      }
      # The app restarts into the new build; wait until each one reports it. It installs in the
      # background and records how that went in get_app_info's update_check (it puts an update off
      # while a sign-in is open, for example), so a machine still answering on the old version says
      # why and is asked again, up to three times in all.
      $waiting = [Collections.Generic.List[string]]::new([string[]]$behind)
      $attempts = @{}
      foreach ($m in $behind) { $attempts[$m] = 1 }
      $asked = Get-Date
      $until = (Get-Date).AddSeconds($InstallWaitSeconds)
      while ($waiting.Count -and (Get-Date) -lt $until) {
        Start-Sleep -Seconds 10
        $now = Invoke-Everywhere $waiting.ToArray() 'get_app_info' 20
        foreach ($m in @($waiting)) {
          if ($now[$m].State -eq 'ok' -and "$($now[$m].Value.version)" -eq $channel) { $null = $waiting.Remove($m); Write-Output "  ${m}: on $channel" }
        }
        if ($waiting.Count -and ((Get-Date) - $asked).TotalSeconds -ge 90) {
          $again = @($waiting | Where-Object { $attempts[$_] -lt 3 -and $now[$_].State -eq 'ok' })
          if ($again) {
            $null = Invoke-Everywhere $again 'install_update'
            foreach ($m in $again) { $attempts[$m]++; Write-Output "  ${m}: still on $($now[$m].Value.version), last check $((Get-LastCheck $now[$m].Value.update_check $null).Text); asked again (attempt $($attempts[$m]))" }
          }
          $asked = Get-Date
        }
      }
      foreach ($m in $waiting) { Write-Output "  ${m}: still not on $channel after $InstallWaitSeconds s" }
      $rows = Get-Report $machines $channel $published
      Write-Output ''
      Write-Report $rows
    }
  }
  $problems = @($rows | Where-Object Problem)
  $summary = 'current', 'behind', 'not installed' | ForEach-Object { $s = $_; "$(@($rows | Where-Object { $_.Status -like "$s*" }).Count) $s" }
  Write-Output "Summary: $($summary -join ', '), $($problems.Count) with problems."
  if ($problems -or ($UpdateNow -and @($rows | Where-Object Behind).Count)) { exit 1 }
  exit 0
} finally {
  # Another process (a scanner) may hold a new file for a moment; Windows then finishes the delete.
  for ($i = 0; $i -lt 10 -and (Test-Path -LiteralPath $scratch); $i++) {
    try { Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction Stop } catch { Start-Sleep -Milliseconds 300 }
  }
}
