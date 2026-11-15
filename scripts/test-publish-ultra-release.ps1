#Requires -Version 7
<#
Tests publish-ultra-release.ps1 offline, on synthetic repositories: a local origin, and a stand-in
for gh that keeps releases in a folder. Nothing reaches GitHub. Build needs the whole toolchain and
is not run here; Sign runs where the tauri CLI is installed (pnpm install), with a throwaway key.

  pwsh scripts/test-publish-ultra-release.ps1
#>
$ErrorActionPreference = 'Stop'
$tmp = Join-Path ([IO.Path]::GetTempPath()) "slopbar-publish-test-$PID"
$realConf = Get-Content (Join-Path $PSScriptRoot '../src-tauri/tauri.conf.json') -Raw | ConvertFrom-Json
. (Join-Path $PSScriptRoot 'publish-ultra-release.ps1') -Repo 'example/ultra' -KeyDir (Join-Path $tmp 'no-key')
# Published data: ultra-v0.1.0-ultra.5's signature, and upstream SlopBar's updater key.
$ultra5Signature = 'dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRRW9xbDFpNG85V1lGdExCT1pldTJlNkJJNDRuRncwbThDMzJGOHZlSnY1OXB6NExFdHUyRE0rNHBrMlJQS0Z3d2R4YjMyVDMvVE5henNnRUViTzg3bDJkQUtSY0FXeWdBPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyODQ5NDY5CWZpbGU6U2xvcEJhcl8wLjEuMC11bHRyYS41X3g2NC1zZXR1cC5leGUKT3A1eTBSTGhiMzBMUkdsdzVUMVFNa0pUU2NKcy82bnV0RC85Y2J3dWcxQmYzWGpFUitteUM0S3FZVFBtS0JESG0zeU9JU3V0TmhWTytFVWw5b25yQlE9PQo='
$upstreamPubkey = 'dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEE0MDdFRDM5QzIxRTZGMTEKUldRUmJ4N0NPZTBIcEcvT2F1YThXMTB0UVRRREpMUmtPSjJUS1V4M2poTzdFUXBtejB0V1FNUFAK'

$failures = 0
function Check([string]$what, [bool]$ok) {
  if ($ok) { Write-Output "ok   $what" } else { Write-Output "FAIL $what"; $script:failures++ }
}
# The message a block stops with, or '' when it runs through.
function Get-Refusal([scriptblock]$block) {
  try { $null = & $block 2>$null; return '' } catch { return $_.Exception.Message }
}

# A synthetic fork: origin (bare) and a checkout of ultra.
$origin = Join-Path $tmp 'origin.git'
$work = Join-Path $tmp 'work'
$store = Join-Path $tmp 'releases'
$null = New-Item -ItemType Directory -Force $tmp, $store, (Join-Path $work 'src-tauri')
function Invoke-Work {
  $out = git -C $work -c user.name=Test -c user.email=test@example.invalid @args
  if ($LASTEXITCODE -ne 0) { throw "git $args failed" }
  $out
}
function Set-Conf([string]$pubkey) {
  [ordered]@{
    version = '0.1.0'
    plugins = @{ updater = @{ pubkey = $pubkey; endpoints = @('https://github.com/example/ultra/releases/download/ultra-latest/latest.json') } }
  } | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $work 'src-tauri/tauri.conf.json')
}

# Stands in for gh: releases live in $store/<tag> (assets/ and release.json).
$secrets = @()
$channelBroken = $false
$onCreate = $null
# Workflow runs, oldest first: databaseId, url, status, conclusion, branch (ultra or a tag) and
# commit. `gh run list` lists them newest first, as gh does, and starts a run for a tag on origin
# the first time it is asked about one (as a pushed tag would). `gh run watch` calls $onWatch with
# the run (to publish, or to be cancelled, as a real run might) and then completes it.
$runs = @()
$onWatch = $null
$watchFails = $false
function Add-Run([string]$branch, [string]$commit, [string]$status = 'queued', [string]$conclusion = '') {
  $id = 100 + @($script:runs).Count
  $run = [pscustomobject]@{ databaseId = $id; url = "https://example.invalid/runs/$id"; status = $status; conclusion = $conclusion; branch = $branch; commit = $commit }
  $script:runs = @($script:runs) + $run
  return $run
}
function gh {
  $ErrorActionPreference = 'Continue'
  $global:LASTEXITCODE = 0
  $pos = @(); $opt = @{}
  for ($i = 0; $i -lt $args.Count; $i++) {
    $a = [string]$args[$i]
    if ($a -match '^(--[a-z-]+)=(.*)$') { $opt[$Matches[1]] = $Matches[2] }
    elseif ($a -cin '-R', '-p', '-D', '--target', '--title', '--notes', '--json', '--jq', '--workflow', '--event', '--branch', '--commit', '--limit', '--interval', '--status') { $opt[$a] = [string]$args[++$i] }
    elseif ($a.StartsWith('-')) { $opt[$a] = $true }
    else { $pos += $a }
  }
  $dir = if ($pos.Count -gt 2) { Join-Path $store $pos[2] }
  $refuse = { param([string]$message) Write-Error $message -ErrorAction Continue; $global:LASTEXITCODE = 1 }
  switch ("$($pos[0]) $($pos[1])") {
    'release download' {
      if ($pos[2] -eq 'ultra-latest' -and $script:channelBroken) { & $refuse 'HTTP 502: Bad Gateway'; return }
      if (-not (Test-Path $dir)) { & $refuse 'release not found'; return }
      $assets = @(Get-ChildItem (Join-Path $dir 'assets') -File | Where-Object { -not $opt['-p'] -or $_.Name -like $opt['-p'] })
      if (-not $assets) { & $refuse 'no assets match the file pattern'; return }
      $null = New-Item -ItemType Directory -Force $opt['-D']
      foreach ($f in $assets) { Copy-Item -LiteralPath $f.FullName (Join-Path $opt['-D'] $f.Name) }
    }
    'release create' {
      if (Test-Path $dir) { & $refuse "a release with the same tag name already exists: $($pos[2])"; return }
      if (-not (git -C $origin rev-parse -q --verify "refs/tags/$($pos[2])")) {
        if ($opt['--verify-tag']) { & $refuse "tag $($pos[2]) doesn't exist in the repo"; return }
        git -C $origin tag $pos[2] $opt['--target']
      }
      $null = New-Item -ItemType Directory -Force (Join-Path $dir 'assets')
      foreach ($f in @($pos | Select-Object -Skip 3)) { Copy-Item -LiteralPath $f (Join-Path $dir 'assets') }
      [ordered]@{
        title = $opt['--title']; notes = $opt['--notes']; latest = $opt['--latest']
        prerelease = [bool]$opt['--prerelease']; verifyTag = [bool]$opt['--verify-tag']
      } | ConvertTo-Json | Set-Content (Join-Path $dir 'release.json')
      if ($script:onCreate) { & $script:onCreate }
    }
    'release upload' {
      if (-not (Test-Path $dir)) { & $refuse 'release not found'; return }
      foreach ($f in @($pos | Select-Object -Skip 3)) { Copy-Item -LiteralPath $f (Join-Path $dir 'assets') -Force }
    }
    'secret list' { $script:secrets }
    'run list' {
      $branch = $opt['--branch']
      if ($branch -like 'ultra-v*' -and -not @($script:runs | Where-Object { $_.branch -eq $branch })) {
        $sha = git -C $origin rev-parse -q --verify "refs/tags/$branch^{commit}"
        if ($sha) { $null = Add-Run $branch $sha }
      }
      $found = @($script:runs | Where-Object {
          (-not $branch -or $_.branch -eq $branch) -and (-not $opt['--commit'] -or $_.commit -eq $opt['--commit']) -and (-not $opt['--status'] -or $_.status -eq $opt['--status']) })
      [array]::Reverse($found)
      $limit = if ($opt['--limit']) { [int]$opt['--limit'] } else { 20 }
      ConvertTo-Json -InputObject @($found | Select-Object -First $limit databaseId, url, status, conclusion) -Compress
    }
    'run watch' {
      # Without --exit-status, gh exits 0 once the run completes, whatever its conclusion.
      $run = @($script:runs | Where-Object { "$($_.databaseId)" -eq $pos[2] })[0]
      if (-not $run -or $script:watchFails) { & $refuse "could not watch run $($pos[2])"; return }
      if ($script:onWatch) { & $script:onWatch $run }
      $run.status = 'completed'
    }
    default { & $refuse "the test's gh does not know: gh $args" }
  }
}

function New-Stage {
  $script:stageDir = Join-Path $tmp "stage-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
  $null = New-Item -ItemType Directory $script:stageDir
}
# Made-up stand-ins for what Build and Sign leave in the stage.
function Add-FakeBuild {
  $release = Read-StageFile 'release.json'
  $setupName = "SlopBar_$($release.version)_x64-setup.exe"
  Set-Content (Join-Path $stageDir $setupName) "installer $($release.version)"
  Set-Content (Join-Path $stageDir "$setupName.sig") "signature $($release.version)"
  [ordered]@{ setup = $setupName; mcpSha256 = 'ab' * 32 } | ConvertTo-Json | Set-Content (Join-Path $stageDir 'build.json')
}
function Get-Release([string]$name) {
  $dir = Join-Path $store $name
  if (-not (Test-Path $dir)) { return $null }
  $release = Get-Content (Join-Path $dir 'release.json') -Raw | ConvertFrom-Json
  $release | Add-Member assets @(Get-ChildItem (Join-Path $dir 'assets') -File | ForEach-Object Name)
  return $release
}
function Get-Channel { Get-Content (Join-Path $store 'ultra-latest/assets/latest.json') -Raw | ConvertFrom-Json }
function Set-ChannelVersion([string]$version) {
  @{ version = $version } | ConvertTo-Json | Set-Content (Join-Path $store 'ultra-latest/assets/latest.json')
}
function Get-OriginTag([string]$name) { git -C $origin rev-parse -q --verify "refs/tags/$name^{commit}" }
function Test-SameNames($a, $b) { -not (Compare-Object @($a) @($b)) -and @($a).Count -eq @($b).Count }

$summary = $env:GITHUB_STEP_SUMMARY
$env:GITHUB_STEP_SUMMARY = $null
try {
  git init -q --bare -b ultra $origin
  git init -q -b ultra $work
  # No background maintenance: it would hold files the cleanup removes.
  foreach ($gitDir in $origin, $work) { git -C $gitDir config gc.auto 0; git -C $gitDir config maintenance.auto false }
  Set-Conf $realConf.plugins.updater.pubkey
  Invoke-Work add -A
  Invoke-Work commit -q -m one
  Invoke-Work remote add origin $origin
  Invoke-Work push -q origin ultra
  $c1 = Invoke-Work rev-parse HEAD
  $root = $work

  # Versions, key ids and the installer check.
  Check 'a later build is newer' (Test-NewerVersion '0.1.0-ultra.6' '0.1.0-ultra.5')
  Check 'build numbers compare as numbers' (Test-NewerVersion '0.1.0-ultra.10' '0.1.0-ultra.9')
  Check 'the same version is not newer' (-not (Test-NewerVersion '0.1.0-ultra.5' '0.1.0-ultra.5'))
  Check 'a later package is newer whatever the build' (Test-NewerVersion '0.2.0-ultra.1' '0.1.0-ultra.9')
  Check 'an earlier package is older whatever the build' (-not (Test-NewerVersion '0.1.0-ultra.9' '0.2.0-ultra.1'))
  Check 'an upstream version is no Ultra version' ((Get-Refusal { Test-NewerVersion '0.2.57' '0.1.0-ultra.1' }) -match 'not an Ultra version')
  $pubkey = $realConf.plugins.updater.pubkey
  Check 'the fork key id is 593D8A8B75A9A204' ((Get-KeyId $pubkey) -eq '04-A2-A9-75-8B-8A-3D-59')
  Check "a published release's signature carries the fork key id" ((Get-KeyId $ultra5Signature) -eq (Get-KeyId $pubkey))
  Check "upstream's key id differs" ((Get-KeyId $upstreamPubkey) -ne (Get-KeyId $pubkey))
  Check 'an installer with the server and the hook passes' (Test-InstallerScript "File /a `"/oname=slopbar-mcp.exe`" `"x`"`n!include `"C:\s\src-tauri\nsis\ultra-hooks.nsh`"")
  Check 'an installer without the hook fails' (-not (Test-InstallerScript 'File /a "/oname=slopbar-mcp.exe" "x"'))
  Check 'an installer without the server fails' (-not (Test-InstallerScript '!include "C:\s\src-tauri\nsis\ultra-hooks.nsh"'))
  # With pnpm on the path (as in the workflow), the command must stay 'pnpm', not 'p'.
  function pnpm { }
  $pnpmCommand = Get-PnpmCommand
  Check 'pnpm on the path runs as pnpm' ($pnpmCommand.Count -eq 1 -and $pnpmCommand[0] -eq 'pnpm')
  Remove-Item Function:\pnpm

  # A machine publishes the pushed ultra tip: build 1, and the first channel.
  New-Stage; $Tag = ''; $Build = 0
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  Check 'a machine takes the next build number' ($r.tag -eq 'ultra-v0.1.0-ultra.1' -and $r.version -eq '0.1.0-ultra.1' -and $r.commit -eq $c1 -and -not $r.pushed -and $null -eq $r.channel)
  Add-FakeBuild
  $null = Invoke-Publish
  $release = Get-Release 'ultra-v0.1.0-ultra.1'
  Check 'the release holds exactly the installer, its .sig and latest.json' (Test-SameNames $release.assets @('SlopBar_0.1.0-ultra.1_x64-setup.exe', 'SlopBar_0.1.0-ultra.1_x64-setup.exe.sig', 'latest.json'))
  Check 'a machine makes the tag with its release, on the released commit' ((Get-OriginTag 'ultra-v0.1.0-ultra.1') -eq $c1 -and -not $release.verifyTag)
  Check 'the release is not marked Latest' ($release.latest -eq 'false' -and -not $release.prerelease -and $release.title -eq 'SlopBar Ultra 0.1.0-ultra.1')
  Check "the notes give slopbar-mcp.exe's SHA-256" ($release.notes -match "slopbar-mcp\.exe SHA-256: $('ab' * 32)")
  $entry = (Get-Channel).platforms.'windows-x86_64'
  Check 'the channel is made, and points at the installer with its signature' ((Get-Channel).version -eq '0.1.0-ultra.1' -and (Get-Release 'ultra-latest').prerelease -and
    $entry.url -eq 'https://github.com/example/ultra/releases/download/ultra-v0.1.0-ultra.1/SlopBar_0.1.0-ultra.1_x64-setup.exe' -and $entry.signature -eq 'signature 0.1.0-ultra.1')

  # A machine refuses an unclean or unpushed checkout, and a build number already taken.
  New-Stage
  Set-Content (Join-Path $work 'stray.txt') 'x'
  Check 'a machine refuses uncommitted changes' ((Get-Refusal { Invoke-Check }) -match 'uncommitted changes')
  Remove-Item (Join-Path $work 'stray.txt')
  Invoke-Work commit -q --allow-empty -m two
  Check 'a machine refuses a commit that is not on origin/ultra' ((Get-Refusal { Invoke-Check }) -match 'is not origin/ultra')
  Invoke-Work push -q origin ultra
  $c2 = Invoke-Work rev-parse HEAD
  $Build = 1
  Check 'a machine refuses a build number already taken' ((Get-Refusal { Invoke-Check }) -match 'not higher than 0\.1\.0-ultra\.1')
  $Build = 0

  # The workflow publishes a tag pushed on its own (as -ViaActions pushes it).
  Invoke-Work push -q origin "$($c2):refs/tags/ultra-v0.1.0-ultra.2"
  Invoke-Work fetch -q origin --tags
  New-Stage; $Tag = 'ultra-v0.1.0-ultra.2'
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  Check 'the workflow takes the version from the tag' ($r.version -eq '0.1.0-ultra.2' -and $r.pushed -and $r.commit -eq $c2 -and $r.channel -eq '0.1.0-ultra.1')
  Add-FakeBuild
  $null = Invoke-Publish
  $release = Get-Release 'ultra-v0.1.0-ultra.2'
  Check 'the workflow publishes on the pushed tag and makes none' ($release.verifyTag -and (Get-OriginTag 'ultra-v0.1.0-ultra.2') -eq $c2)
  Check 'the channel moves to the pushed tag' ((Get-Channel).version -eq '0.1.0-ultra.2')
  Check 'a later step refuses a stage made for another tag' ((Get-Refusal { $Tag = 'ultra-v0.1.0-ultra.3'; Invoke-Publish }) -match 'stage is for ultra-v0\.1\.0-ultra\.2')

  # The workflow refuses tags it must not publish.
  New-Stage
  foreach ($case in @(
      @('ultra-v0.1.0-ultra.07', 'must look like'),
      @('ultra-v0.1-ultra.3', 'must look like'),
      @('ultra-v0.2.0-ultra.3', 'package version is 0.1.0'),
      @('ultra-v0.1.0-ultra.3', 'names no commit here'),
      @('ultra-v0.1.0-ultra.2', 'already holds 0.1.0-ultra.2'))) {
    $Tag = $case[0]
    Check "the workflow refuses $($case[0]) ($($case[1]))" ((Get-Refusal { Invoke-Check }) -match [regex]::Escape($case[1]))
  }
  # Older code under a newer number.
  Invoke-Work push -q origin "$($c1):refs/tags/ultra-v0.1.0-ultra.4"
  Invoke-Work fetch -q origin --tags
  Invoke-Work checkout -q --detach $c1
  $Tag = 'ultra-v0.1.0-ultra.4'
  Check "the workflow refuses a commit older than the channel's" ((Get-Refusal { Invoke-Check }) -match 'does not contain')
  # A commit that is not on ultra.
  Invoke-Work checkout -q --detach $c2
  Invoke-Work commit -q --allow-empty -m side
  Invoke-Work push -q origin "HEAD:refs/tags/ultra-v0.1.0-ultra.5"
  Invoke-Work fetch -q origin --tags
  $Tag = 'ultra-v0.1.0-ultra.5'
  Check 'the workflow refuses a commit that is not on ultra' ((Get-Refusal { Invoke-Check }) -match 'not on the ultra branch')
  Invoke-Work checkout -q ultra
  $Tag = ''

  # A tag that appears on origin while a machine builds belongs to the workflow.
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  Check 'a machine skips build numbers already tagged' ($r.version -eq '0.1.0-ultra.6')
  Add-FakeBuild
  Invoke-Work push -q origin "$($c2):refs/tags/$($r.tag)"
  Check 'a machine leaves a tag pushed meanwhile to the workflow' ((Get-Refusal { Invoke-Publish }) -match 'appeared on origin meanwhile' -and -not (Get-Release $r.tag))

  # The channel moves past this version during the build: nothing is published.
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  Add-FakeBuild
  Set-ChannelVersion '0.1.0-ultra.50'
  Check 'nothing is published once the channel moved past it' ((Get-Refusal { Invoke-Publish }) -match 'already holds 0\.1\.0-ultra\.50' -and -not (Get-Release $r.tag))
  # ... or while it uploads: the channel never moves back.
  Set-ChannelVersion '0.1.0-ultra.2'
  $onCreate = { Set-ChannelVersion '0.1.0-ultra.50' }
  Check 'the channel never moves back to an older release' ((Get-Refusal { Invoke-Publish }) -match 'already holds 0\.1\.0-ultra\.50' -and (Get-Channel).version -eq '0.1.0-ultra.50')
  $onCreate = $null
  Set-ChannelVersion '0.1.0-ultra.2'

  # An unreadable channel stops the run instead of passing for an empty one.
  $channelBroken = $true
  New-Stage
  Check 'an unreadable channel stops the check' ((Get-Refusal { Invoke-Check }) -match 'Could not read the update channel')
  $channelBroken = $false

  # -ViaActions pushes nothing while the workflow could not publish.
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  $secrets = @('ULTRA_UPDATER_KEY')
  Check '-ViaActions refuses while a secret is missing, and pushes no tag' ((Get-Refusal { Invoke-ViaActions }) -match 'ULTRA_UPDATER_KEY_PASSWORD not set' -and -not (Get-OriginTag $r.tag))
  $secrets = @('ULTRA_UPDATER_KEY', 'ULTRA_UPDATER_KEY_PASSWORD', 'ULTRA_TAURI_SIGNING_PRIVATE_KEY')
  Check '-ViaActions refuses while a secret under a retired name exists' ((Get-Refusal { Invoke-ViaActions }) -match 'Delete ULTRA_TAURI_SIGNING_PRIVATE_KEY' -and -not (Get-OriginTag $r.tag))

  # On a push to ultra the workflow decides from what changed since the channel's release.
  function Get-Decision([string]$previous) {
    New-Stage
    $script:Before = $previous
    $null = Invoke-Decide
    Get-Content (Join-Path $stageDir 'mode') -Raw
  }
  function Add-Commit([string]$path, [string]$message) {
    $file = Join-Path $work $path
    $null = New-Item -ItemType Directory -Force (Split-Path -Parent $file)
    Add-Content $file $message
    Invoke-Work add -A
    Invoke-Work commit -q -m $message
    Invoke-Work push -q origin ultra
    Invoke-Work rev-parse HEAD
  }
  $zero = '0' * 40
  Set-ChannelVersion '0.1.0-ultra.2'
  $base = Invoke-Work rev-parse HEAD
  $docs = Add-Commit 'docs/notes.md' 'docs only'
  Check 'a docs-only push publishes nothing' ((Get-Decision $base) -eq 'none' -and (Get-Decision $zero) -eq 'none')
  $tooling = Add-Commit 'scripts/publish-ultra-release.ps1' 'tooling only'
  Check 'a push that changes only the release tooling rehearses' ((Get-Decision $docs) -eq 'rehearsal')
  Check '...also when the previous commit is unknown' ((Get-Decision $zero) -eq 'rehearsal')
  # Every file of the release tooling and of the fleet report rehearses alone, and nothing else does.
  # The loop reads the publisher's own lists, so a file added to one is covered at once. A fleet-only
  # push must rehearse, or its test runs only at the next release, where a break stops that release.
  foreach ($path in @($toolingPaths) + @($fleetPaths)) {
    $previous = Invoke-Work rev-parse HEAD
    $null = Add-Commit $path "only $path"
    $kind = if ($toolingPaths -contains $path) { 'tooling' } else { 'fleet' }
    Check "a push that changes only $path rehearses, and what changed is the $kind" ((Get-Decision $previous) -eq 'rehearsal' -and (Get-Content (Join-Path $stageDir 'changed') -Raw) -eq $kind)
  }
  $previous = Invoke-Work rev-parse HEAD
  $null = Add-Commit 'scripts/measure-footprint.ps1' 'a script no release step runs'
  Check 'a push that changes only another script publishes nothing' ((Get-Decision $previous) -eq 'none' -and (Get-Content (Join-Path $stageDir 'changed') -Raw) -eq 'nothing')
  $previous = Invoke-Work rev-parse HEAD
  $null = Add-Commit 'scripts/fleet-slopbar.ps1' 'the fleet report'
  $null = Add-Commit 'scripts/publish-ultra-release.ps1' 'and the tooling with it'
  Check 'the release tooling counts when the fleet report changed with it' ((Get-Decision $previous) -eq 'rehearsal' -and (Get-Content (Join-Path $stageDir 'changed') -Raw) -eq 'tooling')
  # The lists cover what the build job runs: every script the workflow runs, and the script under each
  # test it runs, and each listed file exists (a renamed one would drop out of its list unseen).
  $repoRoot = Split-Path -Parent $PSScriptRoot
  $listed = @($toolingPaths) + @($fleetPaths)
  $absent = @($listed | Where-Object { -not (Test-Path -LiteralPath (Join-Path $repoRoot $_)) })
  Check "every file in the rehearsal lists exists ($($listed.Count) listed$(if ($absent) { ", missing: $($absent -join ', ')" }))" (-not $absent)
  $workflowText = Get-Content -LiteralPath (Join-Path $repoRoot '.github/workflows/ultra-release.yml') -Raw
  $ran = @([regex]::Matches($workflowText, '(?m)^\s*(?:run:\s*)?\./(scripts/[\w.-]+\.ps1)') | ForEach-Object { $_.Groups[1].Value } | Select-Object -Unique)
  $needed = @(@($ran) + @($ran | ForEach-Object { if ($_ -match '^scripts/test-(.+\.ps1)$') { "scripts/$($Matches[1])" } }) | Select-Object -Unique)
  $unlisted = @($needed | Where-Object { $listed -notcontains $_ })
  Check "every script the workflow runs, and the script under each of its tests, rehearses ($($needed.Count) found$(if ($unlisted) { ", not listed: $($unlisted -join ', ')" }))" ($needed.Count -ge 4 -and -not $unlisted)
  $shipping = Add-Commit 'src/app.js' 'what ships'
  Check 'a push that changes what ships publishes' ((Get-Decision $tooling) -eq 'push')
  $later = Add-Commit 'docs/notes.md' 'docs after a fix'
  Check 'a later docs-only push still publishes a fix not yet released' ((Get-Decision $shipping) -eq 'push')
  # Released up to here: then a change under src-tauri alone ships.
  Invoke-Work push -q origin "$($later):refs/tags/ultra-v0.1.0-ultra.60"
  Set-ChannelVersion '0.1.0-ultra.60'
  Check 'once released, nothing more ships' ((Get-Decision $docs) -eq 'rehearsal' -and (Get-Decision $later) -eq 'none')
  Add-Commit 'src-tauri/icons/note.txt' 'a src-tauri file' | Out-Null
  Check 'anything under src-tauri ships' ((Get-Decision $later) -eq 'push')
  Set-ChannelVersion '0.1.0-ultra.2'

  # The workflow publishes a push to ultra under the next build number, even after ultra moved on.
  New-Stage; $Tag = ''; $Auto = $true
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  $tip = Invoke-Work rev-parse HEAD
  Check 'a push takes the next build number, made with its release' ($r.commit -eq $tip -and -not $r.pushed -and $r.version -eq "0.1.0-ultra.$((Get-HighestBuild '0.1.0' '') + 1)")
  Invoke-Work checkout -q --detach $shipping
  New-Stage
  $null = Invoke-Check
  Check 'a push is published even when ultra has moved on' ((Read-StageFile 'release.json').commit -eq $shipping)
  Invoke-Work commit -q --allow-empty -m 'not on ultra'
  New-Stage
  Check 'a commit that is not on ultra is refused' ((Get-Refusal { Invoke-Check }) -match 'not on the ultra branch')
  Invoke-Work checkout -q ultra
  $Auto = $false
  Check 'the build number counts the channel too' ((Get-HighestBuild '0.1.0' '0.1.0-ultra.70') -eq 70 -and (Get-HighestBuild '0.1.0' '0.2.0-ultra.90') -lt 70)

  # -ViaActions has the workflow publish origin/ultra, and never races a run. A push that changes
  # what ships publishes itself: it follows that run and pushes no tag.
  $secrets = @('ULTRA_UPDATER_KEY', 'ULTRA_UPDATER_KEY_PASSWORD')
  $pollSeconds = 0
  $Before = ''
  $runs = @()
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  $null = Add-Run 'ultra' $r.commit 'in_progress'
  # The push's run publishes under the number it picks, which need not be the one Check picked here.
  $onWatch = { param($run) git -C $origin tag ultra-v0.1.0-ultra.62 $run.commit; Set-ChannelVersion '0.1.0-ultra.62'; $run.conclusion = 'success' }
  $out = Invoke-ViaActions | Out-String
  Check '-ViaActions follows the run of a push that publishes itself, and pushes no tag' ($out -match 'Published 0\.1\.0-ultra\.62' -and -not (Get-OriginTag $r.tag))
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  $out = Invoke-ViaActions | Out-String
  Check '-ViaActions publishes nothing once the channel has the commit' ($out -match 'Nothing to publish' -and -not (Get-OriginTag $r.tag))
  $fix = Add-Commit 'src/app.js' 'another fix'
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  Check '-ViaActions pushes no tag while the push it follows has no run' ((Get-Refusal { Invoke-ViaActions }) -match 'started no release run' -and -not (Get-OriginTag $r.tag))
  # A failed run of the push is left to a re-run: a tag's run would race that.
  $null = Add-Run 'ultra' $fix 'completed' 'failure'
  Check '-ViaActions leaves a failed push run to a re-run, and pushes no tag' ((Get-Refusal { Invoke-ViaActions }) -match 'gh run rerun' -and -not (Get-OriginTag $r.tag))
  # A newer push replaced the waiting run (cancelling it): the newer run publishes the commit too.
  $null = Add-Run 'ultra' $fix 'pending'
  $newer = Add-Commit 'src/app.js' 'a newer fix'
  Invoke-Work checkout -q --detach $fix
  $null = Add-Run 'ultra' $newer 'in_progress'
  $onWatch = {
    param($run)
    if ($run.commit -eq $fix) { $run.conclusion = 'cancelled'; return }
    git -C $origin tag ultra-v0.1.0-ultra.70 $run.commit; Set-ChannelVersion '0.1.0-ultra.70'; $run.conclusion = 'success'
  }
  $out = Invoke-ViaActions | Out-String
  Check '-ViaActions follows the newer push that replaced its run' ($out -match 'Published 0\.1\.0-ultra\.70' -and -not (Get-OriginTag $r.tag))
  Invoke-Work checkout -q ultra
  # A push that ships (W) runs while a docs-only push (X) waits. W's run publishes W, and X's run
  # then finds nothing to publish: X has nothing that ships which the channel lacks.
  $shipW = Add-Commit 'src/app.js' 'ships'
  $docsX = Add-Commit 'docs/notes.md' 'docs after it'
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  $runW = Add-Run 'ultra' $shipW 'in_progress'
  $null = Add-Run 'ultra' $docsX 'pending'
  $onWatch = {
    param($run)
    git -C $origin tag ultra-v0.1.0-ultra.75 $shipW; Set-ChannelVersion '0.1.0-ultra.75'
    $runW.status = 'completed'; $runW.conclusion = 'success'; $run.conclusion = 'success'
  }
  $out = Invoke-ViaActions | Out-String
  Check '-ViaActions publishes nothing more once a newer release has all that ships' ($out -match 'Nothing to publish: nothing that ships' -and -not (Get-OriginTag $r.tag))
  # Documentation alone is never published.
  New-Stage
  $null = Invoke-Check
  $out = Invoke-ViaActions | Out-String
  Check '-ViaActions publishes no documentation-only commit' ($out -match 'Nothing to publish: nothing that ships')
  # Nor the fleet report alone: the workflow rehearses it so its test runs, but no installer holds it,
  # so a tag's release would be the installer the channel has, again.
  $null = Add-Commit 'scripts/fleet-slopbar.ps1' 'the fleet report changed'
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  $out = try { Invoke-ViaActions | Out-String } catch { "refused: $($_.Exception.Message)" }
  Check '-ViaActions publishes nothing, and pushes no tag, for a change of the fleet report alone' ($out -match "Nothing to publish: since the channel's \S+ only the fleet report changed" -and -not (Get-OriginTag $r.tag))
  # A commit whose push publishes nothing although the release tooling changed gets a tag, but only
  # while no run is unfinished, and a failed watch is not taken for a run that did not publish.
  $tooling2 = Add-Commit 'scripts/publish-ultra-release.ps1' 'tooling changed'
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  $pushRun = Add-Run 'ultra' $tooling2 'in_progress'
  Check '-ViaActions pushes no tag while a release run is unfinished' ((Get-Refusal { Invoke-ViaActions }) -match 'has not finished' -and -not (Get-OriginTag $r.tag))
  $pushRun.status = 'completed'; $pushRun.conclusion = 'success'
  $watchFails = $true
  Check '-ViaActions reports a watch that failed as such' ((Get-Refusal { Invoke-ViaActions }) -match 'Could not watch')
  $watchFails = $false
  $onWatch = { param($run) Set-ChannelVersion ($run.branch -replace '^ultra-v'); $run.conclusion = 'success' }
  $out = Invoke-ViaActions | Out-String
  Check '-ViaActions follows its tag, once pushed, to the release' ($out -match "Published $([regex]::Escape($r.version))" -and (Get-OriginTag $r.tag) -eq $tooling2)
  # A tag already on the commit (another run of this) is followed, not doubled; when its run
  # failed, the way out is a re-run.
  $tooling3 = Add-Commit 'scripts/publish-ultra-release.ps1' 'more tooling'
  New-Stage
  $null = Invoke-Check
  $r = Read-StageFile 'release.json'
  Invoke-Work push -q origin "$($tooling3):refs/tags/ultra-v0.1.0-ultra.90"
  $null = Add-Run 'ultra-v0.1.0-ultra.90' $tooling3 'completed' 'failure'
  Check "-ViaActions names the re-run when the tag's run failed" ((Get-Refusal { Invoke-ViaActions }) -match 'gh run rerun' -and -not (Get-OriginTag $r.tag))
  $null = Add-Run 'ultra-v0.1.0-ultra.90' $tooling3 'queued'
  $out = Invoke-ViaActions | Out-String
  Check '-ViaActions follows a tag already on the commit instead of adding one' ($out -match 'Published 0\.1\.0-ultra\.90' -and -not (Get-OriginTag $r.tag))
  $onWatch = $null; $runs = @(); $pollSeconds = 5
  $publisher = Join-Path $PSScriptRoot 'publish-ultra-release.ps1'
  $both = pwsh -NoProfile -File $publisher -Step Check -Tag ultra-v0.1.0-ultra.1 -Auto -Stage (Join-Path $tmp 'args') 2>&1 | Out-String
  $all = pwsh -NoProfile -File $publisher -Auto -Stage (Join-Path $tmp 'args') 2>&1 | Out-String
  Check 'a run is either a tag or a push, one step at a time' ($both -match 'either a pushed tag' -and $all -match 'one -Step at a time')

  # Sign, with a throwaway key: the real signer, its password handoff and the key check.
  if (Test-Path $tauriCli) {
    # (Not $keyDir: PowerShell names ignore case, and $KeyDir is the publisher's.)
    $throwaway = Join-Path $tmp 'key'
    $keyFile = Join-Path $throwaway 'updater.key'
    $null = New-Item -ItemType Directory -Force $throwaway
    $null = node $tauriCli signer generate --ci -p test-password -w $keyFile 2>&1
    Set-Content (Join-Path $throwaway 'updater.key.password') 'test-password'
    New-Stage; $Tag = ''
    [ordered]@{ tag = 'ultra-v0.1.0-ultra.99'; version = '0.1.0-ultra.99'; commit = (Invoke-Work rev-parse HEAD); pushed = $false; channel = $null } |
      ConvertTo-Json | Set-Content (Join-Path $stageDir 'release.json')
    Add-FakeBuild
    $sig = Join-Path $stageDir 'SlopBar_0.1.0-ultra.99_x64-setup.exe.sig'
    $KeyDir = $throwaway
    Check 'a key other than the one in tauri.conf.json is refused' ((Get-Refusal { Invoke-Sign }) -match 'not made with the key in tauri\.conf\.json')
    Set-Conf (Get-Content "$keyFile.pub" -Raw).Trim()
    Check 'the key files sign' ((Get-Refusal { Invoke-Sign }) -eq '' -and (Get-KeyId (Get-Content $sig -Raw)) -eq (Get-KeyId (Get-Content "$keyFile.pub" -Raw)))
    $KeyDir = Join-Path $tmp 'no-key'
    $env:ULTRA_UPDATER_KEY = Get-Content $keyFile -Raw
    $env:ULTRA_UPDATER_KEY_PASSWORD = "test-password`n"
    Check 'the secrets sign, and leave the environment' ((Get-Refusal { Invoke-Sign }) -eq '' -and (Test-Path $sig) -and
      -not $env:ULTRA_UPDATER_KEY -and -not $env:ULTRA_UPDATER_KEY_PASSWORD)
    $env:ULTRA_UPDATER_KEY = Get-Content $keyFile -Raw
    $env:ULTRA_UPDATER_KEY_PASSWORD = 'wrong'
    Check 'a wrong password fails the signing' ((Get-Refusal { Invoke-Sign }) -match 'Signing failed' -and -not (Test-Path $sig))
    Set-Conf $realConf.plugins.updater.pubkey
  } else {
    Write-Output 'skip Sign: no tauri CLI in node_modules (pnpm install)'
  }
} finally {
  $env:GITHUB_STEP_SUMMARY = $summary
  Remove-Item Env:ULTRA_UPDATER_KEY, Env:ULTRA_UPDATER_KEY_PASSWORD -ErrorAction SilentlyContinue
  # Another process (a scanner) may hold a new file for a moment; Windows then finishes the delete
  # once it lets go.
  for ($i = 0; $i -lt 10 -and (Test-Path $tmp); $i++) {
    try { Remove-Item -Recurse -Force $tmp -ErrorAction Stop } catch { Start-Sleep -Milliseconds 500 }
  }
}
if ($failures) { Write-Output "$failures failed"; exit 1 }
Write-Output 'all passed'
exit 0
