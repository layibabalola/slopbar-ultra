#Requires -Version 7
<#
Tests fleet-slopbar.ps1 offline, on synthetic answers only; nothing reaches a machine or GitHub:
every rule that makes a row a problem (each pinned by the row's Problem flag), how it reads each
machine's last update check (get_app_info's update_check) and the channel's release time
(latest.json's pub_date), and its main body, run for real against stand-ins for latest.json and each
machine's door: the line above the table, the summary and the exit code. It also reads two constants
from the Rust source of the checkout (update_check::PASS_LIMIT and ultra_release::CHECK_EVERY) and
fails when the script's copies of them differ.

  pwsh scripts/test-fleet-slopbar.ps1
#>
$ErrorActionPreference = 'Stop'
$fleet = Join-Path $PSScriptRoot 'fleet-slopbar.ps1'
# Dot-sourced without its guard, the script would run a real report.
if ((Get-Content -LiteralPath $fleet -Raw) -notmatch [regex]::Escape("if (`$MyInvocation.InvocationName -eq '.') { return }")) {
  throw 'fleet-slopbar.ps1 must define only its functions when dot-sourced.'
}
. $fleet
$failures = 0
function Check([string]$what, [bool]$ok) {
  if ($ok) { Write-Output "ok   $what" } else { Write-Output "FAIL $what"; $script:failures++ }
}

# A fixed now, and answers as the door gives them, read with ConvertFrom-Json as the script does.
$now = [datetime]::new(2026, 11, 15, 12, 0, 0, [DateTimeKind]::Utc)
function Ago([double]$minutes) { $now.AddMinutes(-$minutes).ToString('yyyy-MM-ddTHH:mm:ssZ', [Globalization.CultureInfo]::InvariantCulture) }
function Record([hashtable]$fields = @{}) {
  $record = [ordered]@{ at = $null; result = $null; version = $null; error = $null; reason = $null; by = $null
    next_at = $null; interval_seconds = 900; pass_limit_seconds = 2140 }
  foreach ($key in $fields.Keys) { $record[$key] = $fields[$key] }
  return $record
}
function Info($record, [string]$version = '0.1.0-ultra.13', [bool]$updates = $true, $keep = $null) {
  $value = [ordered]@{ version = $version; updates_enabled = $updates; pid = 4242 }
  if ($null -ne $record) { $value.update_check = $record }
  if ($null -ne $keep) { $value.keep_running = $keep }
  return [pscustomobject]@{ State = 'ok'; Value = ($value | ConvertTo-Json -Depth 5 | ConvertFrom-Json) }
}
# SlopBar Ultra's watchdog as get_app_info reports it (keep_running): healthy unless told otherwise.
function Keep([hashtable]$fields = @{}) {
  $keep = [ordered]@{ enabled = $true; managed = $true; task = 'ok'; problems = @(); error = $null; last_run = $null; last_result = 0; started_by_watchdog = $false }
  foreach ($key in $fields.Keys) { $keep[$key] = $fields[$key] }
  return $keep
}
$on = [pscustomobject]@{ State = 'ok'; Value = ('{"auto_update":true}' | ConvertFrom-Json) }
$off = [pscustomobject]@{ State = 'ok'; Value = ('{"auto_update":false}' | ConvertFrom-Json) }
$channel = '0.1.0-ultra.13'
function Row($info, $settings = $on, $published = (Ago 120)) { Get-Row 'synthetic-host' $info $settings $channel $published $now }

# Builds from before the record: n/a, no problem in itself; but still behind 15 minutes plus the new
# pass limit (50 2/3 minutes) after the release, their old updater has stalled.
$r = Row (Info $null)
Check 'a build without the record shows n/a and no problem' ($r.LastCheck -eq 'n/a' -and -not $r.Problem -and $r.Status -eq 'current')
$r = Row (Info $null '0.1.0-ultra.10') -published (Ago 600)
Check 'a build without the record, long behind, is a problem' ($r.LastCheck -eq 'n/a' -and $r.Behind -and $r.Problem -and $r.Status -eq 'behind; still behind 600 min after the release')
$r = Row (Info $null '0.1.0-ultra.10') -published (Ago 52)
Check 'a build without the record, 52 minutes behind, is past the grace' ($r.Problem -and $r.Status -eq 'behind; still behind 52 min after the release')
$r = Row (Info $null '0.1.0-ultra.10') -published (Ago 45)
Check 'a build without the record, 45 minutes behind, is within the grace' ($r.Behind -and -not $r.Problem -and $r.Status -eq 'behind')

$r = Row (Info (Record @{ at = (Ago 5); result = 'up_to_date'; by = 'auto'; next_at = (Ago -10) }))
Check 'up to date 5 minutes ago' ($r.LastCheck -eq '5 min ago: up to date' -and -not $r.Problem)
$r = Row (Info (Record @{ at = (Ago 0); result = 'installing'; version = '0.1.0-ultra.14'; next_at = (Ago 0) }))
Check 'installing' ($r.LastCheck -eq '0 min ago: installing 0.1.0-ultra.14' -and -not $r.Problem)

# The rules about the machine itself, each pinned by the row's Problem flag, which the exit code
# follows: a local build, a build without updates, a version newer than the channel's, "Update
# automatically" off, settings that cannot be read, and a machine that does not answer are problems; a
# machine without SlopBar is not. The last check is healthy throughout, so only the rule under test shows.
$healthy = Record @{ at = (Ago 5); result = 'up_to_date'; next_at = (Ago -10) }
$r = Row (Info $healthy '0.1.0')
Check 'a local build is a problem' ($r.Problem -and -not $r.Behind -and $r.Status -eq 'local build: takes no releases')
$r = Row (Info $healthy '0.1.0-ultra.13' $false)
Check 'a build without updates is a problem' ($r.Problem -and -not $r.Behind -and $r.Status -eq 'build without updates')
$r = Row (Info $healthy '0.1.0-ultra.14')
Check 'a build newer than the channel is a problem' ($r.Problem -and -not $r.Behind -and $r.Status -eq 'newer than the channel')
$r = Row (Info $healthy) $off
Check '"Update automatically" off is a problem' ($r.Problem -and $r.AutoUpdate -eq 'off' -and $r.Status -eq 'current; auto-update off')
$r = Row (Info $healthy) ([pscustomobject]@{ State = 'timeout'; Message = 'no answer within 45 s' })
Check 'settings that cannot be read are a problem' ($r.Problem -and $r.AutoUpdate -eq '?' -and $r.Status -eq 'current; settings unreadable (timeout)')
foreach ($state in 'unreachable', 'timeout', 'failed', 'error') {
  $r = Row ([pscustomobject]@{ State = $state; Message = 'the door said so' })
  Check "a machine whose door is $state is a problem, with what it said" ($r.Problem -and -not $r.Behind -and $r.Status -eq "${state}: the door said so")
}
$r = Row ([pscustomobject]@{ State = 'missing' })
Check 'a machine without SlopBar is not installed, which is no problem' (-not $r.Problem -and $r.Status -eq 'not installed')

# SlopBar Ultra's watchdog (keep_running): a build from before it shows n/a; a build that keeps the task
# with the setting on must have it as the app registers it; the setting off, or a build that does not
# keep the task, is no problem.
$r = Row (Info $healthy)
Check 'a build from before the watchdog shows n/a and no problem' ($r.Watchdog -eq 'n/a' -and -not $r.Problem -and $r.Status -eq 'current')
$r = Row (Info $healthy -keep (Keep))
Check 'a watchdog task as it should be is ok and no problem' ($r.Watchdog -eq 'ok' -and -not $r.Problem -and $r.Status -eq 'current')
$r = Row (Info $healthy -keep (Keep @{ task = 'missing' }))
Check 'a missing watchdog task is a problem' ($r.Watchdog -eq 'missing' -and $r.Problem -and $r.Status -eq 'current; watchdog task missing')
$r = Row (Info $healthy -keep (Keep @{ task = 'wrong'; problems = @('logon type Password, not InteractiveToken', 'disabled') }))
Check 'a wrong watchdog task is a problem that says what differs' ($r.Watchdog -eq 'wrong' -and $r.Problem -and
  $r.Status -eq 'current; watchdog task wrong: logon type Password, not InteractiveToken; disabled')
$r = Row (Info $healthy -keep (Keep @{ task = 'unknown'; error = 'Task Scheduler did not answer within 10 s' }))
Check 'a watchdog task that cannot be read is a problem that says why' ($r.Watchdog -eq 'unknown' -and $r.Problem -and
  $r.Status -eq 'current; watchdog task unknown: Task Scheduler did not answer within 10 s')
$r = Row (Info $healthy -keep (Keep @{ enabled = $false; task = 'missing' }))
Check 'the watchdog turned off (keep_running) is no problem' ($r.Watchdog -eq 'off' -and -not $r.Problem -and $r.Status -eq 'current')
$r = Row (Info $healthy -keep (Keep @{ managed = $false; task = 'missing' }))
Check 'a build that does not keep the task is no problem for its absence' ($r.Watchdog -eq 'missing' -and -not $r.Problem -and $r.Status -eq 'current')
$r = Row (Info (Record @{ at = (Ago 1); result = 'error'; error = 'synthetic' ; next_at = (Ago -14) }) -keep (Keep @{ task = 'missing' }))
Check 'a watchdog problem comes after the update check''s' ($r.Problem -and $r.Status -eq 'current; last update check failed; watchdog task missing')

# A failed check: a problem; the table cuts it to 40 characters and the full text follows it.
$failure = 'The download failed: error decoding response body: operation timed out'
$r = Row (Info (Record @{ at = (Ago 1); result = 'error'; error = $failure; next_at = (Ago -1) }))
Check 'a failed check is a problem' ($r.Problem -and $r.Status -eq 'current; last update check failed')
Check 'LastCheck is cut to 40 characters' ($r.LastCheck.Length -eq 40 -and $r.LastCheck.EndsWith('…'))
Check 'the full text is kept' ($r.CheckText -eq "1 min ago: error: $failure")
$printed = Write-Report @($r) | Out-String
Check 'the full text follows the table' ($printed -match [regex]::Escape("synthetic-host: 1 min ago: error: $failure"))
$short = Row (Info (Record @{ at = (Ago 5); result = 'up_to_date'; next_at = (Ago -10) }))
Check 'a check that fits is not listed again' (-not ((Write-Report @($short) | Out-String) -match 'Last checks in full'))

$reason = 'A sign-in is open; the update waits for it to finish'
$r = Row (Info (Record @{ at = (Ago 3); result = 'deferred'; reason = $reason; next_at = (Ago -12) }))
Check 'a deferred check is no problem' (-not $r.Problem -and $r.CheckText -eq "3 min ago: deferred: $reason")

# No check for longer than the interval plus a pass's limit (15 + 35 2/3 = 50 2/3 minutes) is a
# problem when the loop's next run does not explain it: none is shown, or it is overdue past a pass's
# limit and a minute (a stopped loop). A loop that waits for a next run checks there.
$r = Row (Info (Record @{ at = (Ago 52); result = 'up_to_date' }))
Check 'no check for 52 minutes, with no next run shown, is a problem' ($r.Problem -and $r.Status -eq 'current; no update check for 52 min')
$r = Row (Info (Record @{ at = (Ago 49); result = 'up_to_date' }))
Check '49 minutes is within a pass of the interval' (-not $r.Problem)
# With updates off the loop checks nothing, so an old last check is no news: only the setting is reported.
# The record shows no next run, so nothing but the setting stops the old check from being reported too.
$r = Row (Info (Record @{ at = (Ago 52); result = 'up_to_date' })) $off
Check 'with automatic updates off, that alone is reported' ($r.Problem -and $r.Status -eq 'current; auto-update off')
$r = Row (Info (Record @{ at = (Ago 380); result = 'up_to_date'; interval_seconds = 21600 }))
Check 'a 6-hour interval allows longer' (-not $r.Problem)
$r = Row (Info (Record @{ at = (Ago 400); result = 'up_to_date'; interval_seconds = 21600 }))
Check 'but not past it and a pass' ($r.Status -eq 'current; no update check for 400 min')

# "Update automatically" just switched on: the loop skipped its runs while updates were off (it wakes
# at its interval and moves its next run on, but checks nothing), so the last check is old and the
# next run is ahead. That is no problem until the run is overdue; then the loop has stopped.
$r = Row (Info (Record @{ at = (Ago 1500); result = 'up_to_date'; next_at = (Ago -7) }))
Check 'an old check with the next run ahead is no problem (updates just switched on)' (-not $r.Problem -and $r.Status -eq 'current' -and $r.LastCheck -eq '1500 min ago: up to date')
$r = Row (Info (Record @{ at = (Ago 1500); result = 'up_to_date'; next_at = (Ago 5) }))
Check '...nor with the next run just due, or its pass running' (-not $r.Problem)
$r = Row (Info (Record @{ at = (Ago 1500); result = 'up_to_date'; next_at = (Ago 36) }))
Check '...nor 36 minutes overdue, still within a pass and a minute' (-not $r.Problem)
$r = Row (Info (Record @{ at = (Ago 1500); result = 'up_to_date'; next_at = (Ago 40) }))
Check '...but a loop 40 minutes overdue has stopped, and the old check is reported with it' ($r.Problem -and $r.Status -eq 'current; update loop stopped: next check 40 min overdue; no update check for 1500 min')
$r = Row (Info (Record @{ at = (Ago 1500); result = 'up_to_date' })) $off
Check '...and with updates off, only the setting is reported (no next run to explain the old check)' ($r.Problem -and $r.Status -eq 'current; auto-update off')
$r = Row (Info (Record @{ at = (Ago 1500); result = 'up_to_date'; next_at = (Ago 40) })) $off
Check '...but a loop 40 minutes overdue has stopped with updates off too: it wakes at its interval either way' ($r.Problem -and $r.Status -eq 'current; auto-update off; update loop stopped: next check 40 min overdue')
$r = Row (Info (Record @{ at = (Ago 1500); result = 'error'; error = 'The download failed'; next_at = (Ago -7) }))
Check 'a failed check is a problem whatever the next run' ($r.Problem -and $r.Status -eq 'current; last update check failed')

# A stopped loop: its next run overdue by more than a pass's limit and a minute, even after a check
# asked for.
$r = Row (Info (Record @{ at = (Ago 2); result = 'up_to_date'; by = 'manual'; next_at = (Ago 40) }))
Check 'an overdue next run is a stopped loop, whatever a later check shows' ($r.Problem -and $r.Status -eq 'current; update loop stopped: next check 40 min overdue')
$r = Row (Info (Record @{ at = (Ago 20); result = 'found'; version = '0.1.0-ultra.14'; next_at = (Ago 30) }))
Check 'a pass still within its limit is no problem' (-not $r.Problem)
$r = Row (Info (Record @{ next_at = (Ago 37) }))
Check 'no check since start, and the first run long overdue' ($r.LastCheck -eq 'none yet' -and $r.Status -eq 'current; update loop stopped: next check 37 min overdue')
$r = Row (Info (Record @{ next_at = (Ago -0.3) }))
Check 'no check yet, and the first run is due' ($r.LastCheck -eq 'none yet' -and -not $r.Problem)
$r = Row (Info (Record @{ interval_seconds = 21600 }))
Check 'no loop (a build without updates) is no stopped loop' (-not $r.Problem)

# Behind the channel for longer than the interval plus a pass's limit after its release.
$behind = Record @{ at = (Ago 4); result = 'up_to_date'; next_at = (Ago -11) }
$r = Row (Info $behind '0.1.0-ultra.12') -published (Ago 60)
Check 'behind an hour after the release is a problem' ($r.Behind -and $r.Problem -and $r.Status -eq 'behind; still behind 60 min after the release')
$r = Row (Info $behind '0.1.0-ultra.12') -published (Ago 20)
Check 'behind 20 minutes after the release is not yet a problem' ($r.Behind -and -not $r.Problem)

# A release time that cannot be used (latest.json has no pub_date, or one that is not a time): how long
# a machine has been behind cannot be told, so one that is behind is a problem (a stuck one would pass
# for one mid-rollout), and the report goes on. A current machine is not affected. pub_date arrives as
# ConvertFrom-Json reads it: a string, a [datetime], or whatever a malformed file holds.
function Published([string]$json) { ("{`"pub_date`": $json}" | ConvertFrom-Json).pub_date }
# The row, or one without a status when making it threw (so that check fails, not the whole test).
function TryRow($info, $published) {
  try { Row $info -published $published } catch { [pscustomobject]@{ Status = $null; Problem = $false; Behind = $false; Error = $_.Exception.Message } }
}
$noDate = 'behind; release time unknown (latest.json has no pub_date)'
$notATime = 'behind; release time unknown (latest.json has a pub_date that is not a time)'
$r = TryRow (Info $behind '0.1.0-ultra.12') (Published 'null')
Check 'behind, with no pub_date, is a problem that says so' ($r.Behind -and $r.Problem -and $r.Status -eq $noDate)
$r = TryRow (Info $behind '0.1.0-ultra.12') (Published '""')
Check 'behind, with an empty pub_date, is the same' ($r.Behind -and $r.Problem -and $r.Status -eq $noDate)
$r = TryRow (Info $null '0.1.0-ultra.10') (Published 'null')
Check 'a build without the record, behind, with no pub_date, is a problem too' ($r.Behind -and $r.Problem -and $r.Status -eq $noDate)
foreach ($bad in '"soon"', '"11:00"', '"2026-11-15"', '"2026-11-15 11:00"', '"2026-13-45T25:61:61Z"', '1763200000', 'true', '[1, 2]', '{"at": 1}') {
  $r = TryRow (Info $behind '0.1.0-ultra.12') (Published $bad)
  Check "behind, with a pub_date of $bad, is a problem that says so, and no error" ($r.Behind -and $r.Problem -and $r.Status -eq $notATime)
}
$r = TryRow (Info (Record @{ at = (Ago 4); result = 'up_to_date'; next_at = (Ago -11) })) (Published 'null')
Check 'a current machine is not affected by an unusable release time' (-not $r.Behind -and -not $r.Problem -and $r.Status -eq 'current')
$r = TryRow (Info $behind '0.1.0-ultra.12') (Published ('"' + (Ago 60) + '"'))
Check 'a pub_date as ConvertFrom-Json reads it (a [datetime]) is used' ($r.Problem -and $r.Status -eq 'behind; still behind 60 min after the release')
$r = TryRow (Info $behind '0.1.0-ultra.12') (Published ('"' + (Ago 20) + '"'))
Check '...and within the grace it is no problem' ($r.Behind -and -not $r.Problem -and $r.Status -eq 'behind')

# A pub_date ahead of this machine's clock. Within the clock skew ($ClockSkewSeconds, 5 minutes) it is
# the release just published, seen by a slow clock, and counts: the machine is behind, within the grace.
# Further ahead it is a wrong record (a time zone slip, a typo) and no release time: used, it would show
# a stuck machine as plain "behind" until that time came. The limits are pinned to the second.
function Ahead([double]$seconds) { $now.AddSeconds($seconds).ToString('yyyy-MM-ddTHH:mm:ssZ', [Globalization.CultureInfo]::InvariantCulture) }
$future = 'behind; release time unknown (latest.json has a pub_date in the future)'
$r = TryRow (Info $behind '0.1.0-ultra.12') (Published ('"' + (Ahead 240) + '"'))
Check 'a pub_date 4 minutes ahead is within the clock skew: behind, no problem' ($r.Behind -and -not $r.Problem -and $r.Status -eq 'behind')
$r = TryRow (Info $behind '0.1.0-ultra.12') (Published ('"' + (Ahead 300) + '"'))
Check '...and exactly 5 minutes ahead is too' ($r.Behind -and -not $r.Problem -and $r.Status -eq 'behind')
$r = TryRow (Info $behind '0.1.0-ultra.12') (Published ('"' + (Ahead 301) + '"'))
Check 'a pub_date 5 minutes and a second ahead is no release time: a problem that says so' ($r.Behind -and $r.Problem -and $r.Status -eq $future)
$r = TryRow (Info $behind '0.1.0-ultra.12') (Published ('"' + (Ahead 7200) + '"'))
Check '...as is one 2 hours ahead (a time zone slip, say)' ($r.Behind -and $r.Problem -and $r.Status -eq $future)
$r = TryRow (Info $null '0.1.0-ultra.10') (Published ('"' + (Ahead 7200) + '"'))
Check 'a build without the record, behind, with a pub_date 2 hours ahead, is a problem too' ($r.Behind -and $r.Problem -and $r.Status -eq $future)
$r = TryRow (Info $behind '0.1.0-ultra.12') (Ahead 7200)
Check '...also when it arrives as text, not as a time ConvertFrom-Json read' ($r.Behind -and $r.Problem -and $r.Status -eq $future)
$r = TryRow (Info (Record @{ at = (Ago 4); result = 'up_to_date'; next_at = (Ago -11) })) (Published ('"' + (Ahead 7200) + '"'))
Check 'a current machine is not affected by a pub_date ahead' (-not $r.Behind -and -not $r.Problem -and $r.Status -eq 'current')

# Times as the door and latest.json give them: text, or a [datetime] once ConvertFrom-Json read it.
$hourAgo = $now.AddHours(-1)
Check 'a time as text' ((ConvertTo-UtcTime '2026-11-15T11:00:00Z') -eq $hourAgo)
Check 'a time with an offset' ((ConvertTo-UtcTime '2026-11-15T13:00:00+02:00') -eq $hourAgo)
Check 'a time ConvertFrom-Json read' ((ConvertTo-UtcTime ('{"t":"2026-11-15T11:00:00Z"}' | ConvertFrom-Json).t) -eq $hourAgo)
Check 'a time without a kind is UTC' ((ConvertTo-UtcTime ([datetime]::new(2026, 11, 15, 11, 0, 0))) -eq $hourAgo)
$released = Get-ReleaseTime (Published '"2026-11-15T11:00:00Z"') $now
Check 'a release time, as ConvertFrom-Json reads it, is a UTC time' ($released.Time -eq $hourAgo -and $null -eq $released.Why)
$released = Get-ReleaseTime '2026-11-15T13:00:00+02:00' $now
Check 'a release time as text, with an offset' ($released.Time -eq $hourAgo -and $null -eq $released.Why)
# The shape the updater reads, RFC 3339, part by part: fractional seconds, a space or a lower-case t
# between date and time, a lower-case z, and nothing before or after (.NET would read a time with a
# space or a weekday in front, or a space behind it, and the updater would reject the file).
$released = Get-ReleaseTime '2026-11-15T11:00:00.123Z' $now
Check 'a release time with fractional seconds' ($released.Time -eq $hourAgo.AddMilliseconds(123) -and $null -eq $released.Why)
$released = Get-ReleaseTime '2026-11-15 11:00:00Z' $now
Check 'a release time with a space between the date and the time' ($released.Time -eq $hourAgo -and $null -eq $released.Why)
$released = Get-ReleaseTime '2026-11-15t11:00:00z' $now
Check 'a release time with a lower-case t and z' ($released.Time -eq $hourAgo -and $null -eq $released.Why)
foreach ($padded in ' 2026-11-15T11:00:00Z', 'Sun 2026-11-15T11:00:00Z', '2026-11-15T11:00:00Z ') {
  $released = Get-ReleaseTime $padded $now
  Check "a pub_date of '$padded' is not a time" ($null -eq $released.Time -and $released.Why -eq 'latest.json has a pub_date that is not a time')
}
$released = Get-ReleaseTime $null $now
Check 'no pub_date: no release time, and why' ($null -eq $released.Time -and $released.Why -eq 'latest.json has no pub_date')
$released = Get-ReleaseTime '2026-13-45T25:61:61Z' $now
Check 'a pub_date of the right shape that is no time: no release time, and why' ($null -eq $released.Time -and $released.Why -eq 'latest.json has a pub_date that is not a time')
$released = Get-ReleaseTime (Ahead 7200) $now
Check 'a pub_date 2 hours ahead: no release time, and why' ($null -eq $released.Time -and $released.Why -eq 'latest.json has a pub_date in the future')
Check 'the report says so above the table' ((Get-ReleaseTimeNote $null $now) -match '^Release time unknown \(latest\.json has no pub_date\): a machine that is behind is reported as a problem' -and
  (Get-ReleaseTimeNote 'soon' $now) -match 'not a time\)' -and (Get-ReleaseTimeNote (Ahead 7200) $now) -match 'a pub_date in the future\)')
Check 'and says nothing when the release time is known, or just ahead within the skew' ($null -eq (Get-ReleaseTimeNote (Published '"2026-11-15T11:00:00Z"') $now) -and $null -eq (Get-ReleaseTimeNote (Ahead 300) $now))
Check 'short text is left alone' ((Get-Short 'up to date') -eq 'up to date')

# The main body of the script (what runs after its functions: the channel, the machines, the line above
# the table, the summary and the exit code) is run for real, with stand-ins for the only two things that
# leave this computer: latest.json (Invoke-RestMethod) and each machine's door, here or over ssh
# (Start-Process). The stand-ins live only inside Invoke-Report, which also gives the script a synthetic
# name for this machine and a synthetic ssh config. A machine is what its door answers; the clock is the
# real one, so a record's times are relative to now. -UpdateNow, which waits and asks again, is not run.
$reportDir = Join-Path ([IO.Path]::GetTempPath()) "slopbar-fleet-test-$PID"
$null = New-Item -ItemType Directory -Force $reportDir
function RealAgo([double]$minutes) { [datetime]::UtcNow.AddMinutes(-$minutes).ToString('yyyy-MM-ddTHH:mm:ssZ', [Globalization.CultureInfo]::InvariantCulture) }
# A door that answers: this version, with this update record (healthy by default), the setting and the
# watchdog (healthy by default).
function Door([string]$version = '0.1.0-ultra.13', [bool]$auto = $true, $keep = (Keep)) {
  @{ version = $version; auto = $auto; keep = $keep; record = (Record @{ at = (RealAgo 5); result = 'up_to_date'; by = 'auto'; next_at = (RealAgo -10) }) }
}
# $doors: 'here' and the ssh aliases of the other machines, each a Door, or a hashtable with only a
# state: missing (no SlopBar: nothing printed, exit 0) or unreachable (ssh's exit 255).
function Invoke-Report([string]$latestJson, [hashtable]$doors) {
  $sshConfig = Join-Path $reportDir 'ssh_config'
  $aliases = @($doors.Keys | Where-Object { $_ -ne 'here' } | Sort-Object)
  @('# >>> fleet-ssh managed block'; foreach ($a in $aliases) { "Host $a"; "  HostName $a.invalid" }; '# <<< fleet-ssh managed block <<<') | Set-Content -LiteralPath $sshConfig
  ${function:Invoke-RestMethod} = { param($Uri, $TimeoutSec) $latestJson | ConvertFrom-Json }.GetNewClosure()
  ${function:Start-Process} = {
    param($FilePath, $ArgumentList, $RedirectStandardOutput, $RedirectStandardError, [switch]$NoNewWindow, [switch]$PassThru)
    # ssh: -n -o BatchMode=yes -o ConnectTimeout=10 <machine> cmd /d /c if exist <door> <door> --call <tool>
    $words = @("$ArgumentList" -split '\s+')
    $door = $doors[$(if ("$FilePath" -eq 'ssh') { $words[5] } else { 'here' })]
    $code = 0; $text = ''; $message = ''
    if ($door.state -eq 'unreachable') { $code = 255; $message = 'Connection timed out' }
    elseif ($door.state -ne 'missing') {
      $answer = switch ($words[-1]) {
        'get_app_info' { [ordered]@{ version = $door.version; updates_enabled = $true; pid = 4242; update_check = $door.record; keep_running = $door.keep } }
        'get_settings' { [ordered]@{ auto_update = $door.auto } }
      }
      $text = @{ ok = $answer } | ConvertTo-Json -Depth 6 -Compress
    }
    Set-Content -LiteralPath $RedirectStandardOutput -Value $text -NoNewline
    Set-Content -LiteralPath $RedirectStandardError -Value $message -NoNewline
    $process = [pscustomobject]@{ Id = 0; Handle = 1; ExitCode = $code }
    $process | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value { param($milliseconds) $true }
    $process
  }.GetNewClosure()
  # The script's own exit code is what is read here, and none is left behind: a caller that exits with
  # $LASTEXITCODE (as GitHub's pwsh steps do) would take the script's 1 for this test's.
  $savedName = $env:COMPUTERNAME
  $savedExit = $global:LASTEXITCODE
  $env:COMPUTERNAME = 'DESK-1'
  $global:LASTEXITCODE = $null
  try {
    $lines = @(& $fleet -SshConfig $sshConfig -TimeoutSeconds 5 2>&1 | ForEach-Object { "$_" })
    return [pscustomobject]@{ Exit = $LASTEXITCODE; Lines = @($lines -join "`n" -split "`r?`n") }
  } catch {
    return [pscustomobject]@{ Exit = $null; Lines = @("THREW: $($_.Exception.Message)") }
  } finally {
    $env:COMPUTERNAME = $savedName
    $global:LASTEXITCODE = $savedExit
  }
}
function Latest([string]$pubDate = '') { if ($pubDate) { "{`"version`":`"$channel`",`"pub_date`":`"$pubDate`"}" } else { "{`"version`":`"$channel`"}" } }
# How many lines of a report match a pattern, and where the first one is (-1 when none).
function Matching($report, [string]$pattern) { @($report.Lines | Where-Object { $_ -match $pattern }).Count }
function LineOf($report, [string]$pattern) { for ($i = 0; $i -lt $report.Lines.Count; $i++) { if ($report.Lines[$i] -match $pattern) { return $i } }; return -1 }
function Summary($report) { @($report.Lines | Where-Object { $_ -like 'Summary: *' }) -join ' / ' }
$noPubDate = 'Release time unknown (latest.json has no pub_date): a machine that is behind is reported as a problem, because how long it has been behind cannot be told.'
$exitBefore = $global:LASTEXITCODE

try {
  $report = Invoke-Report (Latest (RealAgo 120)) @{ here = (Door); 'box-1' = (Door); 'lap-1' = (Door) }
  Check 'a fleet that is all current says its channel and machines, summarises them, and exits 0' ($report.Exit -eq 0 -and
    (Matching $report '^Channel: 0\.1\.0-ultra\.13\. Machines: 3 \(this one and the ssh managed block\)\.$') -eq 1 -and (Summary $report) -eq 'Summary: 3 current, 0 behind, 0 not installed, 0 with problems.')
  Check '...lists each machine, this one by its name' ((Matching $report '^(desk-1 \(here\)|box-1|lap-1)\s+0\.1\.0-ultra\.13\s+on\s+5 min ago: up to date\s+ok\s+current\s*$') -eq 3)
  Check '...under a Watchdog column' ((Matching $report '^Machine\s+Version\s+AutoUpdate\s+LastCheck\s+Watchdog\s+Status\s*$') -eq 1)
  Check '...and has no line about the release time while it is known' ((Matching $report '^Release time unknown') -eq 0)

  $report = Invoke-Report (Latest) @{ here = (Door); 'box-1' = (Door) }
  $note = LineOf $report '^Release time unknown'
  Check 'a latest.json without a pub_date is reported above the table, in full' ($note -ge 0 -and $note -lt (LineOf $report 'Machine\s+Version\s+AutoUpdate') -and $report.Lines[$note] -eq $noPubDate)
  Check '...and fails nothing while every machine is current' ($report.Exit -eq 0 -and (Summary $report) -eq 'Summary: 2 current, 0 behind, 0 not installed, 0 with problems.')
  $report = Invoke-Report (Latest (RealAgo 120)) @{ here = (Door); 'box-1' = (Door '0.1.0-ultra.12') }
  Check 'a machine long behind the channel is a problem, and the exit code says so' ($report.Exit -eq 1 -and (Summary $report) -eq 'Summary: 1 current, 1 behind, 0 not installed, 1 with problems.' -and
    (Matching $report '^box-1\s+0\.1\.0-ultra\.12\s+on\s.*\sbehind; still behind 1[12]\d min after the release\s*$') -eq 1)
  $report = Invoke-Report (Latest (RealAgo 20)) @{ here = (Door); 'box-1' = (Door '0.1.0-ultra.12') }
  Check 'a machine behind for less than the grace is no problem: exit 0' ($report.Exit -eq 0 -and (Summary $report) -eq 'Summary: 1 current, 1 behind, 0 not installed, 0 with problems.')
  $report = Invoke-Report (Latest) @{ here = (Door); 'box-1' = (Door '0.1.0-ultra.12') }
  Check 'a machine behind when the release time is unknown is a problem: the line, its row and exit 1' ($report.Exit -eq 1 -and (Matching $report '^Release time unknown') -eq 1 -and $report.Lines -contains $noPubDate -and
    (Matching $report '^box-1\s.*\sbehind; release time unknown \(latest\.json has no pub_date\)\s*$') -eq 1 -and (Summary $report) -eq 'Summary: 1 current, 1 behind, 0 not installed, 1 with problems.')
  $twoHoursAhead = [datetime]::UtcNow.AddHours(2).ToString('yyyy-MM-ddTHH:mm:ssZ', [Globalization.CultureInfo]::InvariantCulture)
  $report = Invoke-Report (Latest $twoHoursAhead) @{ here = (Door); 'box-1' = (Door '0.1.0-ultra.12') }
  Check 'a pub_date 2 hours ahead of the clock is reported above the table, and a machine behind is a problem' ($report.Exit -eq 1 -and
    (Matching $report '^Release time unknown \(latest\.json has a pub_date in the future\): a machine that is behind') -eq 1 -and (Matching $report '^box-1\s.*\sbehind; release time unknown \(latest\.json has a pub_date in the future\)\s*$') -eq 1)
  $report = Invoke-Report (Latest (RealAgo 120)) @{ here = (Door); 'box-1' = @{ state = 'unreachable' } }
  Check 'a machine that does not answer is a problem: exit 1' ($report.Exit -eq 1 -and (Summary $report) -eq 'Summary: 1 current, 0 behind, 0 not installed, 1 with problems.' -and
    (Matching $report '^box-1\s+unreachable: Connection timed out\s*$') -eq 1)
  $report = Invoke-Report (Latest (RealAgo 120)) @{ here = (Door); 'box-1' = @{ state = 'missing' } }
  Check 'a machine without SlopBar is not installed, which is no problem: exit 0' ($report.Exit -eq 0 -and (Summary $report) -eq 'Summary: 1 current, 0 behind, 1 not installed, 0 with problems.' -and
    (Matching $report '^box-1\s+not installed\s*$') -eq 1)
  $report = Invoke-Report (Latest (RealAgo 120)) @{ here = (Door '0.1.0-ultra.13' $false); 'box-1' = (Door) }
  Check 'a machine with "Update automatically" off is a problem: exit 1' ($report.Exit -eq 1 -and (Summary $report) -eq 'Summary: 2 current, 0 behind, 0 not installed, 1 with problems.')
  $report = Invoke-Report (Latest (RealAgo 120)) @{ here = (Door); 'box-1' = (Door -keep (Keep @{ task = 'missing' })); 'lap-1' = (Door -keep $null) }
  Check 'a machine whose watchdog task is missing is a problem: its row and exit 1' ($report.Exit -eq 1 -and (Summary $report) -eq 'Summary: 3 current, 0 behind, 0 not installed, 1 with problems.' -and
    (Matching $report '^box-1\s+0\.1\.0-ultra\.13\s+on\s+5 min ago: up to date\s+missing\s+current; watchdog task missing\s*$') -eq 1 -and
    (Matching $report '^lap-1\s+0\.1\.0-ultra\.13\s+on\s+5 min ago: up to date\s+n/a\s+current\s*$') -eq 1)
} finally {
  # The report folder goes when the scenarios are done. Another process (a scanner) may hold a new file
  # for a moment; Windows then finishes the delete.
  for ($i = 0; $i -lt 10 -and (Test-Path -LiteralPath $reportDir); $i++) {
    try { Remove-Item -LiteralPath $reportDir -Recurse -Force -ErrorAction Stop } catch { Start-Sleep -Milliseconds 300 }
  }
}
Check 'the runs leave no exit code of the script behind (a pwsh step would take it for this test''s)' ($LASTEXITCODE -eq $exitBefore)

# A machine that reports neither its update interval nor its pass limit (a build from before the
# record, or a record from before the limit) is judged by Ultra's own: the script holds copies of
# ultra_release::CHECK_EVERY and update_check::PASS_LIMIT. Nothing but this test ties the copies to
# the app, so it reads both from the Rust source (the release build's checkout) and fails when they
# differ. A copy must change together with its constant.
#
# The constants are Duration::from_secs(<seconds>): whole numbers with + and *, and other constants'
# .as_secs(), possibly over several lines. Anything else throws, so a constant rewritten in a form this
# cannot read fails the test instead of passing it unread.
function Get-RustSeconds([string]$source, [string]$name, [int]$depth = 0) {
  if ($depth -gt 8) { throw "$name refers to other constants too deeply." }
  $pattern = "(?s)\bconst\s+$([regex]::Escape($name))\s*:\s*(?:std::time::)?Duration\s*=\s*(?:std::time::)?Duration::from_secs\((?<expr>[^;]*?)\)\s*;"
  $found = [regex]::Match($source, $pattern)
  if (-not $found.Success) { throw "The source has no const $name of the form Duration::from_secs(...)." }
  $expr = $found.Groups['expr'].Value.Trim().TrimEnd(',').Trim()
  $expr = [regex]::Replace($expr, '\b([A-Z][A-Z0-9_]*)\.as_secs\(\)', { param($m) '(' + (Get-RustSeconds $source $m.Groups[1].Value ($depth + 1)) + ')' })
  if ($expr -notmatch '^[\d\s+*()]+$') { throw "$name is '$expr', which this test cannot read." }
  return [long][Data.DataTable]::new().Compute($expr, $null)
}
function Throws([scriptblock]$block) { try { $null = & $block; return $false } catch { return $true } }
# The seconds, or $null when it cannot be read (so a check on a sample fails instead of the whole test).
function TrySeconds([string]$source, [string]$name) { try { Get-RustSeconds $source $name } catch { $null } }

$rust = @'
pub const FIRST: Duration = Duration::from_secs(40);
pub const HALF_HOUR_AND_A_BIT: Duration = Duration::from_secs(31 * 60);
pub const QUARTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);
pub const TOTAL: Duration = Duration::from_secs(
    FIRST.as_secs() + HALF_HOUR_AND_A_BIT.as_secs() + 2 * 60 + 60,
);
pub const IN_MILLIS: Duration = Duration::from_millis(500);
pub const COMPUTED: Duration = Duration::from_secs(minutes() * 60);
pub const LOOP_A: Duration = Duration::from_secs(LOOP_B.as_secs());
pub const LOOP_B: Duration = Duration::from_secs(LOOP_A.as_secs());
'@
Check 'a constant in seconds is read' ((TrySeconds $rust 'FIRST') -eq 40)
Check 'a product is read' ((TrySeconds $rust 'HALF_HOUR_AND_A_BIT') -eq 1860)
Check 'std::time:: paths are read' ((TrySeconds $rust 'QUARTER') -eq 900)
Check 'a sum over other constants, over lines, with a trailing comma, is read' ((TrySeconds $rust 'TOTAL') -eq 40 + 1860 + 120 + 60)
Check 'the end or the start of another constant''s name is not that constant' ((Throws { Get-RustSeconds $rust 'IRST' }) -and (Throws { Get-RustSeconds $rust 'FIR' }))
foreach ($unreadable in 'ABSENT', 'IN_MILLIS', 'COMPUTED', 'LOOP_A') {
  Check "$unreadable, which this cannot read, stops the test" (Throws { Get-RustSeconds $rust $unreadable })
}

$source = Join-Path $PSScriptRoot '../src-tauri/src'
$passLimit = Get-RustSeconds (Get-Content -LiteralPath (Join-Path $source 'update_check.rs') -Raw) 'PASS_LIMIT'
$checkEvery = Get-RustSeconds (Get-Content -LiteralPath (Join-Path $source 'ultra_release.rs') -Raw) 'CHECK_EVERY'
Check "the pass limit of a machine that reports none is update_check::PASS_LIMIT (the script has $OldBuildPassLimit s, the source $passLimit s)" ($OldBuildPassLimit -eq $passLimit)
Check "the interval of a machine that reports none is ultra_release::CHECK_EVERY (the script has $OldBuildInterval s, the source $checkEvery s)" ($OldBuildInterval -eq $checkEvery)
Check 'no fallback is a number written out again' ((Get-Content -LiteralPath $fleet -Raw) -notmatch '\?\?\s*\d')

# What a machine reports wins; without it, the two values above are used, in a record as in a row.
$bare = Record @{ at = (Ago 5); result = 'up_to_date'; next_at = (Ago -10) }
$bare.Remove('interval_seconds'); $bare.Remove('pass_limit_seconds')
$last = Get-LastCheck (Info $bare).Value.update_check $true $now
Check 'a record without the limits is judged by the two values' ($last.Interval -eq $OldBuildInterval -and $last.PassLimit -eq $OldBuildPassLimit)
$last = Get-LastCheck (Info (Record @{ at = (Ago 5); result = 'up_to_date'; next_at = (Ago -10); interval_seconds = 21600; pass_limit_seconds = 600 })).Value.update_check $true $now
Check "a machine's own interval and pass limit win" ($last.Interval -eq 21600 -and $last.PassLimit -eq 600)
$r = Row (Info (Record @{ at = (Ago 2); result = 'up_to_date'; next_at = (Ago 12); pass_limit_seconds = 600 }))
Check "a machine's own pass limit sets when its loop has stopped (12 minutes overdue, limit 10 min)" ($r.Problem -and $r.Status -eq 'current; update loop stopped: next check 12 min overdue')
$r = Row (Info (Record @{ at = (Ago 2); result = 'up_to_date'; next_at = (Ago 12) }))
Check '...where the default limit would still wait' (-not $r.Problem)

if ($failures) { Write-Output "$failures failed"; exit 1 }
Write-Output 'all passed'
exit 0
