#Requires -Version 7
<#
Tests fleet-slopbar.ps1 offline: how it reads each machine's last update check (get_app_info's
update_check) and the problems it reports, on synthetic answers only. Nothing reaches a machine or
GitHub. It also reads two constants from the Rust source of the checkout (update_check::PASS_LIMIT
and ultra_release::CHECK_EVERY) and fails when the script's copies of them differ.

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
function Info($record, [string]$version = '0.1.0-ultra.13') {
  $value = [ordered]@{ version = $version; updates_enabled = $true; pid = 4242 }
  if ($null -ne $record) { $value.update_check = $record }
  return [pscustomobject]@{ State = 'ok'; Value = ($value | ConvertTo-Json -Depth 5 | ConvertFrom-Json) }
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
# limit (a stopped loop). A loop that waits for a next run checks there.
$r = Row (Info (Record @{ at = (Ago 52); result = 'up_to_date' }))
Check 'no check for 52 minutes, with no next run shown, is a problem' ($r.Problem -and $r.Status -eq 'current; no update check for 52 min')
$r = Row (Info (Record @{ at = (Ago 49); result = 'up_to_date' }))
Check '49 minutes is within a pass of the interval' (-not $r.Problem)
$r = Row (Info (Record @{ at = (Ago 52); result = 'up_to_date'; next_at = (Ago -1) })) $off
Check 'with automatic updates off, that alone is reported' ($r.Status -eq 'current; auto-update off')
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
$r = Row (Info (Record @{ at = (Ago 1500); result = 'up_to_date'; next_at = (Ago -7) })) $off
Check '...and with updates off, only the setting is reported' ($r.Status -eq 'current; auto-update off')
$r = Row (Info (Record @{ at = (Ago 1500); result = 'error'; error = 'The download failed'; next_at = (Ago -7) }))
Check 'a failed check is a problem whatever the next run' ($r.Problem -and $r.Status -eq 'current; last update check failed')

# A stopped loop: its next run overdue by more than a pass's limit, even after a check asked for.
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

# Times as the door and latest.json give them: text, or a [datetime] once ConvertFrom-Json read it.
$hourAgo = $now.AddHours(-1)
Check 'a time as text' ((ConvertTo-UtcTime '2026-11-15T11:00:00Z') -eq $hourAgo)
Check 'a time with an offset' ((ConvertTo-UtcTime '2026-11-15T13:00:00+02:00') -eq $hourAgo)
Check 'a time ConvertFrom-Json read' ((ConvertTo-UtcTime ('{"t":"2026-11-15T11:00:00Z"}' | ConvertFrom-Json).t) -eq $hourAgo)
Check 'a time without a kind is UTC' ((ConvertTo-UtcTime ([datetime]::new(2026, 11, 15, 11, 0, 0))) -eq $hourAgo)
$released = Get-ReleaseTime (Published '"2026-11-15T11:00:00Z"')
Check 'a release time, as ConvertFrom-Json reads it, is a UTC time' ($released.Time -eq $hourAgo -and $null -eq $released.Why)
$released = Get-ReleaseTime '2026-11-15T13:00:00+02:00'
Check 'a release time as text, with an offset' ($released.Time -eq $hourAgo -and $null -eq $released.Why)
$released = Get-ReleaseTime $null
Check 'no pub_date: no release time, and why' ($null -eq $released.Time -and $released.Why -eq 'latest.json has no pub_date')
$released = Get-ReleaseTime '2026-13-45T25:61:61Z'
Check 'a pub_date of the right shape that is no time: no release time, and why' ($null -eq $released.Time -and $released.Why -eq 'latest.json has a pub_date that is not a time')
Check 'the report says so above the table' ((Get-ReleaseTimeNote $null) -match '^Release time unknown \(latest\.json has no pub_date\): a machine that is behind is reported as a problem' -and
  (Get-ReleaseTimeNote 'soon') -match 'not a time\)')
Check 'and says nothing when the release time is known' ($null -eq (Get-ReleaseTimeNote (Published '"2026-11-15T11:00:00Z"')))
Check 'short text is left alone' ((Get-Short 'up to date') -eq 'up to date')

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
