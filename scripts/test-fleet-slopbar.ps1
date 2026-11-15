#Requires -Version 7
<#
Tests fleet-slopbar.ps1 offline: how it reads each machine's last update check (get_app_info's
update_check) and the problems it reports, on synthetic answers only. Nothing reaches a machine or
GitHub.

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

# No check for longer than the interval plus a pass's limit: 15 + 35 2/3 = 50 2/3 minutes.
$r = Row (Info (Record @{ at = (Ago 52); result = 'up_to_date'; next_at = (Ago -1) }))
Check 'no check for 52 minutes is a problem' ($r.Problem -and $r.Status -eq 'current; no update check for 52 min')
$r = Row (Info (Record @{ at = (Ago 49); result = 'up_to_date'; next_at = (Ago -1) }))
Check '49 minutes is within a pass of the interval' (-not $r.Problem)
$r = Row (Info (Record @{ at = (Ago 52); result = 'up_to_date'; next_at = (Ago -1) })) $off
Check 'with automatic updates off, that alone is reported' ($r.Status -eq 'current; auto-update off')
$r = Row (Info (Record @{ at = (Ago 380); result = 'up_to_date'; next_at = (Ago -1); interval_seconds = 21600 }))
Check 'a 6-hour interval allows longer' (-not $r.Problem)
$r = Row (Info (Record @{ at = (Ago 400); result = 'up_to_date'; next_at = (Ago -1); interval_seconds = 21600 }))
Check 'but not past it and a pass' ($r.Status -eq 'current; no update check for 400 min')

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
$r = Row (Info $behind '0.1.0-ultra.12') -published $null
Check 'without a release time there is no such problem' ($r.Behind -and -not $r.Problem)

# Times as the door and latest.json give them: text, or a [datetime] once ConvertFrom-Json read it.
$hourAgo = $now.AddHours(-1)
Check 'a time as text' ((ConvertTo-UtcTime '2026-11-15T11:00:00Z') -eq $hourAgo)
Check 'a time with an offset' ((ConvertTo-UtcTime '2026-11-15T13:00:00+02:00') -eq $hourAgo)
Check 'a time ConvertFrom-Json read' ((ConvertTo-UtcTime ('{"t":"2026-11-15T11:00:00Z"}' | ConvertFrom-Json).t) -eq $hourAgo)
Check 'a time without a kind is UTC' ((ConvertTo-UtcTime ([datetime]::new(2026, 11, 15, 11, 0, 0))) -eq $hourAgo)
Check 'short text is left alone' ((Get-Short 'up to date') -eq 'up to date')

if ($failures) { Write-Output "$failures failed"; exit 1 }
Write-Output 'all passed'
