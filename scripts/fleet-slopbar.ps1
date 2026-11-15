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
by each one that was cut, in full. A build from before that record shows n/a there, which is no
problem in itself, because machines update one by one. The limits below come from each machine's own
record: its update interval and the limit of one pass of its update loop. A machine that reports
none (a build from before the record) is judged by Ultra's own two values ($OldBuildInterval and
$OldBuildPassLimit below), which the test reads from the Rust source, failing when they differ.

Problems (exit code 1): a local build (a version without -ultra., which takes no releases), a build
without updates, "Update automatically" off, a machine that does not answer, an update that does not
arrive, a machine still behind the channel longer than the interval plus a pass's limit after the
channel's release was published (latest.json's pub_date; a build from before the record, whose old
updater could stall, gets Ultra's 15-minute interval plus the new pass limit), a machine that is
behind when the release time is unknown (no pub_date, one that is not a time, or one further ahead of
this machine's clock than $ClockSkewSeconds below allows: how long it has been behind cannot be
told, and a stuck machine must not pass for one mid-rollout; the report says so and goes on, and a
machine that is current is not affected), and, from the record:
  - a failed last update check (a deferred one is no problem);
  - no update check for longer than the interval plus a pass's limit while "Update automatically" is
    on, unless the loop's next run explains it (it is ahead, or not yet a pass's limit and a minute
    overdue: a machine on which updates were just switched on checks there);
  - an update loop whose next run is overdue by more than a pass's limit and a minute: it has
    stopped, even if a check asked for since then shows a recent time, and with updates off too (the
    loop wakes at its interval either way).
A machine that has just woken from a long sleep shows both of the last two until its next pass, and
cannot be told from a dead loop: ask again after an interval (docs/ULTRA-RELEASES.md says how long a
sleep it takes).
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
# What a machine that does not report them is judged by, in seconds: Ultra's update interval
# (ultra_release::CHECK_EVERY) and the limit of one pass of the update loop (update_check::PASS_LIMIT).
# A build from before the update-check record has neither, and a record from before the limit has no
# limit. A machine that reports its own is judged by those. scripts/test-fleet-slopbar.ps1 reads both
# constants from the Rust source and fails when these two differ, so change them together.
$OldBuildInterval = 900
$OldBuildPassLimit = 2140
# How far ahead of this machine's clock latest.json's pub_date may be and still count, in seconds. The
# publisher stamps it from its own clock (a GitHub runner's, or the key machine's) and this report
# reads it against this machine's, so a slow clock here shows a release just published as ahead.
# Synchronised clocks differ by a second or so, and 5 minutes is what Windows itself tolerates between
# computers (the Kerberos default), so a time ahead by no more than that is the release just
# published, and is used as it is: the machine is behind, within the grace. A time further ahead is no
# skew but a wrong record (a time zone or daylight saving slip is at least half an hour; a typo is
# more), and it is no release time: used, it would show a stuck machine as plain "behind" until that
# time came.
$ClockSkewSeconds = 300

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

# When the channel's release was published: latest.json's pub_date, as a UTC time (Time), or why it
# cannot be used (Why, with Time $null): it is missing, not a time, or ahead of $now by more than
# $ClockSkewSeconds. $published is what ConvertFrom-Json made of it: text, a [datetime], or whatever a
# malformed file holds. The updater reads it as RFC 3339 (a date, a time and an offset) and rejects
# the whole file otherwise, so text that is not that is no time here either, whatever .NET would make
# of it ("11:00" is today, and so is " 2026-11-15T11:00:00Z" with a space in front).
function Get-ReleaseTime($published, [datetime]$now = [datetime]::UtcNow) {
  $unusable = { param([string]$why) [pscustomobject]@{ Time = $null; Why = $why } }
  if ($null -eq $published -or "$published".Trim() -eq '') { return & $unusable 'latest.json has no pub_date' }
  $notATime = 'latest.json has a pub_date that is not a time'
  if ($published -isnot [datetime] -and "$published" -notmatch '^\d{4}-\d{2}-\d{2}[Tt ]\d{2}:\d{2}:\d{2}(\.\d+)?([Zz]|[+-]\d{2}:\d{2})$') { return & $unusable $notATime }
  try { $time = ConvertTo-UtcTime $published } catch { return & $unusable $notATime }
  if (($time - $now).TotalSeconds -gt $ClockSkewSeconds) { return & $unusable 'latest.json has a pub_date in the future' }
  return [pscustomobject]@{ Time = $time; Why = $null }
}

# The line above the table when the release time cannot be used, else $null.
function Get-ReleaseTimeNote($published, [datetime]$now = [datetime]::UtcNow) {
  $release = Get-ReleaseTime $published $now
  if (-not $release.Why) { return $null }
  return "Release time unknown ($($release.Why)): a machine that is behind is reported as a problem, because how long it has been behind cannot be told."
}

# $text cut to $max characters.
function Get-Short([string]$text, [int]$max = 40) {
  if ($text.Length -le $max) { return $text }
  return $text.Substring(0, $max - 1) + '…'
}

# The app's last update check (get_app_info's update_check): its text, the problems it shows, and
# the machine's update interval and pass limit, in seconds. A build from before the record has no
# update_check: n/a, no problem. Problems: a failed check (a deferred one is none); an update loop
# whose next run is overdue by more than a pass's limit and a minute, which means it has stopped,
# whatever a check asked for since then shows and whether or not updates are on; and no check for
# longer than the interval plus a pass's limit while "Update automatically" is on, unless the loop's
# next run explains it. The loop wakes at its interval even while updates are off (and moves its next
# run on), but checks only when they are on, so a machine on which they were just switched on has an
# old last check and a next run ahead, where it checks. That is no problem; only a loop that has
# stopped, or shows no next run, has nothing to explain an old check. (A machine just woken from a
# long sleep looks like a stopped loop, and nothing in the record tells them apart: see
# docs/ULTRA-RELEASES.md.)
function Get-LastCheck($check, $auto, [datetime]$now = [datetime]::UtcNow) {
  if ($null -eq $check) { return [pscustomobject]@{ Text = 'n/a'; Problems = @(); Interval = $null; PassLimit = $null } }
  $interval = [double]($check.interval_seconds ?? $OldBuildInterval)
  $passLimit = [double]($check.pass_limit_seconds ?? $OldBuildPassLimit)
  $problems = [Collections.Generic.List[string]]::new()
  $due = ConvertTo-UtcTime $check.next_at
  $stopped = $null -ne $due -and ($now - $due).TotalSeconds -gt $passLimit + 60
  if ($stopped) {
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
  elseif ($auto -ne $false -and $age.TotalSeconds -gt $interval + $passLimit -and ($stopped -or $null -eq $due)) { $problems.Add("no update check for $minutes min") }
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
# pub_date, as ConvertFrom-Json read it: see Get-ReleaseTime).
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
      # Still behind long after the release: its update loop should have installed it by now. A build
      # from before the record, whose old updater could stall, gets Ultra's interval and the new pass
      # limit, so a machine stuck on it shows during a rollout. Without a usable release time (none, not
      # a time, or ahead of this clock beyond the skew) that cannot be told, and a machine that is stuck
      # would pass for one mid-rollout: a problem too.
      if ($row.Behind) {
        $release = Get-ReleaseTime $published $now
        if ($null -eq $release.Time) {
          $notes.Add("release time unknown ($($release.Why))"); $row.Problem = $true
        } else {
          $since = $now - $release.Time
          if ($since.TotalSeconds -gt ($last.Interval ?? $OldBuildInterval) + ($last.PassLimit ?? $OldBuildPassLimit)) {
            $notes.Add("still behind $([int][Math]::Floor($since.TotalMinutes)) min after the release"); $row.Problem = $true
          }
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
  $releaseNote = Get-ReleaseTimeNote $published
  if ($releaseNote) { Write-Output $releaseNote }
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
