#Requires -Version 7
<#
Publishes a SlopBar Ultra release and moves the update channel (release ultra-latest) to it. A run
on a machine and the release workflow (.github/workflows/ultra-release.yml) run the same steps of
this script, so they build, check, sign and publish alike. See docs/ULTRA-RELEASES.md.

Normally nobody runs it: every push to ultra that changes what ships ($shippingPaths) is published
by the workflow. By hand:
  pwsh scripts/publish-ultra-release.ps1              # on the machine with the updater key
  pwsh scripts/publish-ultra-release.ps1 -Build 7     # a given build number (higher than any before)
  pwsh scripts/publish-ultra-release.ps1 -ViaActions  # any machine: the workflow publishes origin/ultra
The workflow, one step per call, sharing the -Stage folder:
  -Step Decide -Before <sha>             on a push to ultra: publish, rehearse, or nothing (mode and changed files)
  -Step Check -Auto | -Tag <tag>         a push to ultra, or a pushed tag
  -Step Build|Sign|Publish [-Tag <tag>]

Decide   Publish when anything under $shippingPaths changed since the channel's release; rehearse
         (the build job without publishing: Check, Build and the offline tests) when only the release
         tooling ($toolingPaths) or the fleet report ($fleetPaths) changed in this push; else nothing.
         -ViaActions releases a rehearsal's commit by tag only for the tooling: no installer holds the
         fleet report.
Check    The checkout is clean. By hand: it is exactly origin/ultra. On a push: the pushed commit, on
         ultra. Both take the next free build number, above every ultra-v tag and the channel. On a
         tag: the tagged commit, on ultra, under the tag's version. Always: the version is newer than
         the channel's, and the commit contains the one the channel's release was built from.
Build    Unit tests twice: without the desktop app, and with the features that ship (ultra-updates).
         Then the installer with ultra-updates, the installer hook that sets a running
         slopbar-mcp.exe aside, and no signing key anywhere in the environment. The executable must
         report a clean build of this commit; the installer must carry slopbar-mcp.exe with the hook.
Sign     `tauri signer sign`. Only the signer's process gets the key's password (and, in the
         workflow, the key), through its environment; neither is printed or passed as an argument.
         The signature must be from the key in tauri.conf.json.
Publish  Release ultra-v<version> with exactly the installer, its .sig and latest.json, downloaded
         again and compared. Then the channel moves to it and is read back. The channel is checked
         again before both, in case another release moved it meanwhile.

Steps share files through -Stage (by default a new folder in %TEMP%), never the repository.
#>
param(
  [int]$Build = 0,
  [switch]$ViaActions,
  [string]$Tag = '',
  [switch]$Auto,
  [string]$Before = '',
  [ValidateSet('All', 'Decide', 'Check', 'Build', 'Sign', 'Publish')]
  [string]$Step = 'All',
  [string]$Stage = '',
  [string]$Repo = 'layibabalola/slopbar-ultra',
  [string]$KeyDir = (Join-Path $HOME '.slopbar-ultra')
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$tauriCli = Join-Path $root 'node_modules/@tauri-apps/cli/tauri.js'
$channelTag = 'ultra-latest'
$workflow = 'ultra-release.yml'
# Seconds between looks for a workflow run that has not appeared yet.
$pollSeconds = 5
# What ships: everything the installer is built from. A change here is published.
$shippingPaths = @('src', 'src-tauri', 'package.json', 'pnpm-lock.yaml')
# The release tooling: a change here alone rehearses the build, and -ViaActions releases it by tag,
# since the build it makes may differ.
$toolingPaths = @('.github/workflows/ultra-release.yml', 'scripts/publish-ultra-release.ps1', 'scripts/test-publish-ultra-release.ps1')
# The fleet report and its test: the build job runs the test, but no installer holds either. A change
# here alone rehearses too, so its test runs on that push and not at the next release, which a break
# would stop; -ViaActions publishes nothing for it, as for documentation. Everything the workflow runs
# is in one of the two lists: the publisher's test fails otherwise.
$fleetPaths = @('scripts/fleet-slopbar.ps1', 'scripts/test-fleet-slopbar.ps1')
# The workflow hands these secrets to the Sign step alone, under the same names.
$secretNames = @('ULTRA_UPDATER_KEY', 'ULTRA_UPDATER_KEY_PASSWORD')
# Never to be set: the release workflow of older commits reads these names, and it publishes without
# the installer hook or any of these checks.
$retiredSecretNames = @('ULTRA_TAURI_SIGNING_PRIVATE_KEY', 'ULTRA_TAURI_SIGNING_PRIVATE_KEY_PASSWORD')

function Invoke-Checked([string]$what, [scriptblock]$block) {
  & $block
  if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)." }
}

# The minisign key id: bytes 2..9 of the key or signature line (after the comment line).
function Get-KeyId([string]$base64) {
  $text = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($base64.Trim()))
  $line = ($text -split "`r?`n")[1]
  $bytes = [Convert]::FromBase64String($line)
  return [BitConverter]::ToString($bytes[2..9])
}

# Whether Ultra version $a (<x.y.z>-ultra.<n>) is newer than $b.
function Test-NewerVersion([string]$a, [string]$b) {
  $pattern = '^(\d+\.\d+\.\d+)-ultra\.(\d+)$'
  if ($a -notmatch $pattern) { throw "$a is not an Ultra version." }
  $packageA = [version]$Matches[1]; $buildA = [int]$Matches[2]
  if ($b -notmatch $pattern) { throw "$b is not an Ultra version." }
  $packageB = [version]$Matches[1]; $buildB = [int]$Matches[2]
  return $packageA -gt $packageB -or ($packageA -eq $packageB -and $buildA -gt $buildB)
}

# Whether the generated installer script packs slopbar-mcp.exe and includes the hook.
function Test-InstallerScript([string]$nsi) {
  return $nsi -match '/oname=slopbar-mcp\.exe' -and ($nsi -match 'slopbar-mcp\.old-' -or $nsi -match 'ultra-hooks\.nsh')
}

# pnpm itself, or through corepack, always as an array: a lone 'pnpm' out of an `if` arrives as a
# string, and [0] of a string is its first letter.
function Get-PnpmCommand {
  $command = @(if (Get-Command pnpm -ErrorAction SilentlyContinue) { 'pnpm' } else { 'corepack', 'pnpm' })
  return , $command
}

function Read-Config {
  $conf = Get-Content (Join-Path $root 'src-tauri/tauri.conf.json') -Raw | ConvertFrom-Json
  if ($conf.version -notmatch '^\d+\.\d+\.\d+$') { throw "Package version $($conf.version) is not x.y.z." }
  if (@($conf.plugins.updater.endpoints) -join ' ' -ne "https://github.com/$Repo/releases/download/$channelTag/latest.json") {
    throw 'The updater must point only at the Ultra update channel.'
  }
  return $conf
}

function Read-StageFile([string]$name) {
  $path = Join-Path $stageDir $name
  if (-not (Test-Path -LiteralPath $path)) { throw "$path is missing; run the earlier steps with this -Stage first." }
  return Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
}

# Later steps must work on the commit (and, in the workflow, the tag) that Check approved.
function Assert-StageMatches($release) {
  if ($Tag -and $release.tag -ne $Tag) { throw "The stage is for $($release.tag), not $Tag." }
  $head = (git -C $root rev-parse HEAD).Trim()
  if ($head -ne $release.commit) { throw "The stage is for $($release.commit), but the checkout is $head." }
}

function Write-Summary([string[]]$lines) {
  if ($env:GITHUB_STEP_SUMMARY) { Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY -Value $lines -Encoding utf8 }
}

# Downloads the channel's latest.json into a new folder and returns its path, or $null when the
# channel release does not exist yet. Any other failure stops the run: an unreadable channel must
# never pass for an empty one.
function Save-Channel {
  $dir = Join-Path $stageDir "channel-$([guid]::NewGuid().ToString('N'))"
  $log = "$dir.log"
  $null = gh release download $channelTag -R $Repo -p latest.json -D $dir 2> $log
  if ($LASTEXITCODE -eq 0) { return Join-Path $dir 'latest.json' }
  $message = "$(Get-Content -LiteralPath $log -Raw -ErrorAction SilentlyContinue)".Trim()
  if ($message -match 'release not found') { return $null }
  throw "Could not read the update channel: $message"
}

# The version the channel holds, or $null when there is no channel yet.
function Get-ChannelVersion {
  $path = Save-Channel
  if (-not $path) { return $null }
  $current = (Get-Content -LiteralPath $path -Raw | ConvertFrom-Json).version
  if ($current -notmatch '^\d+\.\d+\.\d+-ultra\.\d+$') { throw "The channel holds $current, not an Ultra version." }
  return $current
}

function Assert-Newer([string]$version, [string]$current) {
  if ($current -and -not (Test-NewerVersion $version $current)) { throw "The channel already holds $current; $version would not be newer." }
}

# Refuses $version unless it is newer than the channel's, as read now. Returns the channel's version,
# or $null when there is no channel yet.
function Assert-NewerThanChannel([string]$version) {
  $current = Get-ChannelVersion
  Assert-Newer $version $current
  return $current
}

# The commit a local tag names; it must exist.
function Get-TagCommit([string]$name) {
  $commit = git -C $root rev-parse -q --verify "refs/tags/$name^{commit}"
  if ($LASTEXITCODE -ne 0 -or -not $commit) { throw "Tag $name is not in this checkout." }
  return $commit
}

function Test-Ancestor([string]$ancestor, [string]$commit) {
  git -C $root merge-base --is-ancestor $ancestor $commit 2>$null
  return $LASTEXITCODE -eq 0
}

# Whether anything under $paths differs between two commits.
function Test-Changed([string]$from, [string]$to, [string[]]$paths) {
  git -C $root diff --quiet $from $to -- @paths
  if ($LASTEXITCODE -gt 1) { throw "git diff $from $to failed (exit $LASTEXITCODE)." }
  return $LASTEXITCODE -eq 1
}

# Refuses a commit that lacks the one the channel's release was built from, so a newer build
# number can never ship older code.
function Assert-ContainsChannel([string]$commit, [string]$current) {
  if (-not $current) { return }
  $built = Get-TagCommit "ultra-v$current"
  if (-not (Test-Ancestor $built $commit)) { throw "$commit does not contain $built, which the channel's $current was built from." }
}

# The commit a tag on origin names, or $null when origin has no such tag.
function Get-RemoteTagCommit([string]$name) {
  $lines = @(git -C $root ls-remote origin "refs/tags/$name" "refs/tags/$name^{}")
  if ($LASTEXITCODE -ne 0) { throw "Could not read tag $name from origin." }
  $peeled = @($lines | Where-Object { $_ -like "*`trefs/tags/$name^{}" })
  $plain = @($lines | Where-Object { $_ -like "*`trefs/tags/$name" })
  $line = @($peeled + $plain)[0]
  if (-not $line) { return $null }
  return ($line -split "`t")[0]
}

# The highest build number taken for this package: by an ultra-v tag on origin, or by the channel.
# $refs: `git ls-remote --tags` lines already read, so a caller can count the tags it just looked at.
function Get-HighestBuild([string]$package, [string]$current, [string[]]$refs = $null) {
  if ($null -eq $refs) {
    $refs = @(git -C $root ls-remote --tags origin "refs/tags/ultra-v$package-ultra.*")
    if ($LASTEXITCODE -ne 0) { throw 'Could not list the ultra-v tags on origin.' }
  }
  $taken = @($refs | ForEach-Object { if ($_ -match "refs/tags/ultra-v$([regex]::Escape($package))-ultra\.(\d+)$") { [int]$Matches[1] } })
  if ($current -match '^(\d+\.\d+\.\d+)-ultra\.(\d+)$' -and $Matches[1] -eq $package) { $taken += [int]$Matches[2] }
  return [int](($taken | Measure-Object -Maximum).Maximum ?? 0)
}

# 0. On a push to ultra (the workflow): publish, rehearse or nothing. Compared with the channel's
# release, not the previous push, so the next run catches up whatever an earlier one did not publish.
# Writes the mode, and what changed ('shipping', 'tooling', 'fleet' or 'nothing': the first that
# applies), which tells -ViaActions whether a rehearsal's commit is one to release (see $fleetPaths).
function Invoke-Decide {
  # The channel first: fetched after, its release's tag is here even if one lands in between.
  $current = Get-ChannelVersion
  Invoke-Checked 'git fetch' { git -C $root fetch -q origin ultra --tags }
  $head = (git -C $root rev-parse HEAD).Trim()
  $built = if ($current) { Get-TagCommit "ultra-v$current" }
  if (-not $built -or (Test-Changed $built $head $shippingPaths)) {
    $mode = 'push'
    $changed = 'shipping'
    $why = if ($built) { "what ships changed since $current" } else { 'no release yet' }
  } else {
    # The tooling or the fleet report alone changed in this push (or, without a usable previous
    # commit, since the release).
    $since = if ($Before -match '^[0-9a-f]{40}$' -and $Before -notmatch '^0+$' -and (Test-Ancestor $Before $head)) { $Before } else { $built }
    $tooling = Test-Changed $since $head $toolingPaths
    $fleet = Test-Changed $since $head $fleetPaths
    $mode = if ($tooling -or $fleet) { 'rehearsal' } else { 'none' }
    $changed = if ($tooling) { 'tooling' } elseif ($fleet) { 'fleet' } else { 'nothing' }
    $why = if ($tooling -and $fleet) { 'only the release tooling and the fleet report changed' }
      elseif ($tooling) { 'only the release tooling changed' }
      elseif ($fleet) { 'only the fleet report changed' }
      else { "nothing that ships changed since $current" }
  }
  Set-Content -LiteralPath (Join-Path $stageDir 'mode') -Value $mode -NoNewline
  Set-Content -LiteralPath (Join-Path $stageDir 'changed') -Value $changed -NoNewline
  Write-Output "Decision for $($head): $mode ($why)."
}

# 1. Source and version.
function Invoke-Check {
  $conf = Read-Config
  $package = $conf.version
  # The channel, then origin/ultra and every tag, the channel's included (a fresh fetch, whatever
  # the checkout fetched; after the channel, so its tag is here even if a release lands in between).
  $current = Get-ChannelVersion
  Invoke-Checked 'git fetch' { git -C $root fetch -q origin ultra --tags }
  if (git -C $root status --porcelain) { throw 'The checkout has uncommitted changes.' }
  $head = (git -C $root rev-parse HEAD).Trim()
  if ($Tag) {
    # A pushed tag names the version, and the checkout is its commit.
    if ($Tag -notmatch '^ultra-v(\d+\.\d+\.\d+)-ultra\.([1-9]\d*)$') {
      throw "Tag $Tag must look like ultra-v<package version>-ultra.<build>, e.g. ultra-v$package-ultra.1."
    }
    if ($Matches[1] -ne $package) { throw "Tag $Tag says $($Matches[1]) but the package version is $package." }
    $version = "$($Matches[1])-ultra.$($Matches[2])"
    $name = $Tag
    $tagged = git -C $root rev-parse -q --verify "refs/tags/$Tag^{commit}"
    if ($tagged -ne $head) { throw "The checkout is $head, but tag $Tag names $($tagged ?? 'no commit here')." }
    if (-not (Test-Ancestor $head origin/ultra)) { throw "$head is not on the ultra branch." }
  } else {
    if ($Auto) {
      # A push to ultra: the pushed commit (ultra may have moved on since; later runs publish that).
      if (-not (Test-Ancestor $head origin/ultra)) { throw "$head is not on the ultra branch." }
    } elseif ($head -ne (git -C $root rev-parse origin/ultra).Trim()) {
      throw "HEAD $head is not origin/ultra; release only the pushed ultra tip."
    }
    $highest = Get-HighestBuild $package $current
    $n = if ($Build -eq 0) { $highest + 1 } else { $Build }
    if ($n -le $highest) { throw "Build $n is not higher than $package-ultra.$highest, the highest taken." }
    $version = "$package-ultra.$n"
    $name = "ultra-v$version"
  }
  Assert-Newer $version $current
  Assert-ContainsChannel $head $current
  [ordered]@{ tag = $name; version = $version; commit = $head; pushed = [bool]$Tag; channel = $current } |
    ConvertTo-Json | Set-Content -LiteralPath (Join-Path $stageDir 'release.json')
  Write-Output "Publishing $version from $head (channel now: $($current ?? 'none'))."
}

# 2. Build, with no signing key anywhere in this process: the build runs third-party build scripts.
function Invoke-Build {
  $release = Read-StageFile 'release.json'
  Assert-StageMatches $release
  $version = $release.version
  # `tauri build` rewrites Cargo.toml with LF line ends. A CRLF checkout (core.autocrlf) changes size,
  # so git, and the build's dirty check, would call the clean tree modified. Write it as LF first and
  # restage it (the same blob) so git knows that size.
  $manifestPath = Join-Path $root 'src-tauri/Cargo.toml'
  $text = [IO.File]::ReadAllText($manifestPath)
  if ($text.Contains("`r`n")) { [IO.File]::WriteAllText($manifestPath, $text.Replace("`r`n", "`n")) }
  Invoke-Checked 'git add Cargo.toml' { git -C $root add src-tauri/Cargo.toml }
  if (git -C $root status --porcelain) { throw 'The checkout differs from HEAD beyond the line ends of Cargo.toml.' }
  $signingVariables = @('TAURI_SIGNING_PRIVATE_KEY', 'TAURI_SIGNING_PRIVATE_KEY_PATH', 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD') + $secretNames
  foreach ($n in $signingVariables) { [Environment]::SetEnvironmentVariable($n, $null) }
  if (-not (Get-Command node -ErrorAction SilentlyContinue)) { $env:Path = "$env:ProgramFiles\nodejs;$env:Path" }
  # The installer hook sets a running slopbar-mcp.exe aside so the new one is written (agents'
  # servers lock it; a quiet install would skip it). Sign makes the updater signature afterwards.
  $override = Join-Path $stageDir 'build.conf.json'
  @{
    version = $version
    bundle  = @{
      createUpdaterArtifacts = $false
      windows                = @{ nsis = @{ installerHooks = (Join-Path $root 'src-tauri/nsis/ultra-hooks.nsh') } }
    }
  } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $override
  $pnpm = Get-PnpmCommand
  $pnpmArgs = @($pnpm | Select-Object -Skip 1)
  # The cargo features that ship, for the build and for a second test run: a test can pass without
  # them and fail with them. `tauri build` also turns on tauri/custom-protocol, which no test reads.
  # A new test configuration here needs a new rust-cache key in ultra-release.yml.
  $features = 'ultra-updates'
  Push-Location $root
  try {
    Invoke-Checked 'pnpm install' { & $pnpm[0] @pnpmArgs install --frozen-lockfile }
    Invoke-Checked 'Unit tests without the desktop app' { cargo test --locked --no-default-features --lib --manifest-path src-tauri/Cargo.toml }
    Invoke-Checked "Unit tests with $features" { cargo test --locked --features $features --lib --manifest-path src-tauri/Cargo.toml }
    Invoke-Checked 'Build' { & $pnpm[0] @pnpmArgs tauri build --features $features --config $override }
  } finally { Pop-Location }

  $out = Join-Path $root 'src-tauri/target/release'
  $infoPath = Join-Path $stageDir 'build-info.json'
  $p = Start-Process -FilePath (Join-Path $out 'slopbar.exe') -ArgumentList "--build-info-to `"$infoPath`"" -WindowStyle Hidden -Wait -PassThru
  if ($p.ExitCode -ne 0) { throw 'Build metadata command failed.' }
  $info = Get-Content -LiteralPath $infoPath -Raw | ConvertFrom-Json
  if ($info.commit -ne $release.commit -or $info.dirty -or $info.tauri_dev -ne $false -or -not $info.upstream_updates) {
    throw "The built executable is not a clean ultra-updates build of $($release.commit)."
  }
  # The installer packs agents' MCP server from here, with the hook that sets a running one aside.
  $mcpHash = (Get-FileHash (Join-Path $out 'slopbar-mcp.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
  if (-not (Test-InstallerScript (Get-Content (Join-Path $out 'nsis/x64/installer.nsi') -Raw))) {
    throw 'The installer does not carry slopbar-mcp.exe with the hook that sets a running one aside.'
  }
  $setupName = "SlopBar_${version}_x64-setup.exe"
  Copy-Item (Join-Path $out "bundle/nsis/$setupName") (Join-Path $stageDir $setupName)
  [ordered]@{ setup = $setupName; mcpSha256 = $mcpHash } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $stageDir 'build.json')
  Write-Output "Built $setupName from $($release.commit); slopbar-mcp.exe SHA-256 $mcpHash."
  Write-Summary @("Built and checked ``$setupName`` from $($release.commit).", '', "slopbar-mcp.exe SHA-256: ``$mcpHash``")
}

# 3. Sign. The key's password (and a key given as text) reach only the signer's environment.
function Invoke-Sign {
  $release = Read-StageFile 'release.json'
  Assert-StageMatches $release
  $built = Read-StageFile 'build.json'
  $setup = Join-Path $stageDir $built.setup
  $pubkey = (Read-Config).plugins.updater.pubkey
  # In the workflow the key comes from the secrets; on a machine, from its key files.
  $keyText = "$([Environment]::GetEnvironmentVariable($secretNames[0]))".Trim()
  $password = "$([Environment]::GetEnvironmentVariable($secretNames[1]))".TrimEnd("`r", "`n")
  foreach ($n in $secretNames) { [Environment]::SetEnvironmentVariable($n, $null) }
  $signerArgs = @($tauriCli, 'signer', 'sign')
  if ($keyText) {
    if (-not $password) { throw "$($secretNames[1]) is empty." }
    $source = 'the ULTRA_ secrets'
  } else {
    $keyPath = Join-Path $KeyDir 'updater.key'
    $passwordPath = Join-Path $KeyDir 'updater.key.password'
    if (-not (Test-Path $keyPath) -or -not (Test-Path $passwordPath)) { throw "No updater key in $KeyDir." }
    $password = (Get-Content $passwordPath -Raw).TrimEnd("`r", "`n")
    $signerArgs += @('--private-key-path', $keyPath)
    $source = 'the key and password files'
  }
  Remove-Item -LiteralPath "$setup.sig" -ErrorAction SilentlyContinue
  $start = [Diagnostics.ProcessStartInfo]::new((Get-Command node).Source)
  foreach ($a in $signerArgs + $setup) { $start.ArgumentList.Add($a) }
  $start.UseShellExecute = $false
  # No console to ask on: a prompt for the password gets end of input and fails.
  $start.RedirectStandardInput = $true
  $start.RedirectStandardOutput = $true
  $start.RedirectStandardError = $true
  foreach ($n in 'TAURI_SIGNING_PRIVATE_KEY', 'TAURI_SIGNING_PRIVATE_KEY_PATH', 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD') { $null = $start.Environment.Remove($n) }
  if ($keyText) { $start.Environment['TAURI_SIGNING_PRIVATE_KEY'] = $keyText }
  $start.Environment['TAURI_SIGNING_PRIVATE_KEY_PASSWORD'] = $password
  try {
    $signer = [Diagnostics.Process]::Start($start)
    $signer.StandardInput.Close()
    $null = $signer.StandardOutput.ReadToEnd(); $null = $signer.StandardError.ReadToEnd()
    $signer.WaitForExit()
    if ($signer.ExitCode -ne 0) { throw "Signing failed (exit $($signer.ExitCode)); check $source." }
  } finally {
    $null = $start.Environment.Remove('TAURI_SIGNING_PRIVATE_KEY')
    $null = $start.Environment.Remove('TAURI_SIGNING_PRIVATE_KEY_PASSWORD')
    $start = $null; $keyText = $null; $password = $null
  }
  $signature = (Get-Content -LiteralPath "$setup.sig" -Raw).Trim()
  if ((Get-KeyId $signature) -ne (Get-KeyId $pubkey)) { throw 'The signature was not made with the key in tauri.conf.json.' }
  Write-Output "Signed $($built.setup) with the key in tauri.conf.json."
}

# 4. Publish the version, then move the channel to it.
function Invoke-Publish {
  $release = Read-StageFile 'release.json'
  Assert-StageMatches $release
  $built = Read-StageFile 'build.json'
  $version = $release.version; $name = $release.tag; $head = $release.commit
  $setup = Join-Path $stageDir $built.setup
  $signature = (Get-Content -LiteralPath "$setup.sig" -Raw).Trim()
  # Another release may have moved the channel while this one was built.
  $null = Assert-NewerThanChannel $version
  $manifest = [ordered]@{
    version   = $version
    notes     = "SlopBar Ultra $version, built from $head on the ultra branch."
    pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'Z'")
    platforms = [ordered]@{
      'windows-x86_64' = [ordered]@{ signature = $signature; url = "https://github.com/$Repo/releases/download/$name/$($built.setup)" }
    }
  }
  $latest = Join-Path $stageDir 'latest.json'
  $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $latest -Encoding utf8NoBOM
  $files = @($setup, "$setup.sig", $latest)
  $title = "SlopBar Ultra $version"
  $notes = "SlopBar Ultra $version, built from $head on the ultra branch. Installs update themselves from this fork only.`n`nslopbar-mcp.exe SHA-256: $($built.mcpSha256)"
  if ($release.pushed) {
    # The workflow publishes a tag pushed on its own; --verify-tag never makes one.
    Invoke-Checked 'gh release create' { gh release create $name @files -R $Repo --verify-tag --latest=false --title $title --notes $notes }
  } else {
    # The tag appears together with its release, so the workflow finds the release and stands down.
    if (Get-RemoteTagCommit $name) { throw "Tag $name appeared on origin meanwhile, so the workflow publishes it. Publish again for the next build number." }
    Invoke-Checked 'gh release create' { gh release create $name @files -R $Repo --target $head --latest=false --title $title --notes $notes }
  }
  $tagged = Get-RemoteTagCommit $name
  if ($tagged -ne $head) { throw "Release $name is on $($tagged ?? 'no tag'), not $head. Delete it, then publish the next build number." }
  $check = Join-Path $stageDir 'check'
  Remove-Item -LiteralPath $check -Recurse -Force -ErrorAction SilentlyContinue
  Invoke-Checked 'Download of the release' { gh release download $name -R $Repo -D $check }
  $expected = @($files | ForEach-Object { Split-Path -Leaf $_ } | Sort-Object)
  $found = @(Get-ChildItem -LiteralPath $check -File | ForEach-Object Name | Sort-Object)
  if (($found -join '|') -ne ($expected -join '|')) { throw "Release $name holds $($found -join ', '), not exactly $($expected -join ', ')." }
  foreach ($f in $files) {
    $back = Join-Path $check (Split-Path -Leaf $f)
    if ((Get-FileHash -LiteralPath $f).Hash -ne (Get-FileHash -LiteralPath $back).Hash) { throw "$(Split-Path -Leaf $f) on the release differs from the one built." }
  }

  # Installs follow the channel: check it once more right before moving it, so it never goes back.
  $current = Assert-NewerThanChannel $version
  if (-not $current) {
    Invoke-Checked 'Creating the channel' {
      gh release create $channelTag -R $Repo --target $head --prerelease --latest=false --title 'SlopBar Ultra update channel' `
        --notes 'Holds latest.json for automatic updates of SlopBar Ultra. Download the installer from the newest ultra-v release instead.'
    }
  }
  Invoke-Checked 'Moving the channel' { gh release upload $channelTag $latest -R $Repo --clobber }
  $back = Save-Channel
  if (-not $back -or (Get-FileHash -LiteralPath $latest).Hash -ne (Get-FileHash -LiteralPath $back).Hash) { throw 'The channel does not hold this latest.json.' }

  $lines = @(
    "Published $version"
    "  release: https://github.com/$Repo/releases/tag/$name"
    "  channel: https://github.com/$Repo/releases/download/$channelTag/latest.json"
    "  installer: $setup"
    "  slopbar-mcp.exe sha256: $($built.mcpSha256)"
  )
  Write-Output $lines
  Write-Summary @("Published [$version](https://github.com/$Repo/releases/tag/$name) and moved the update channel to it.")
}

# The newest release run for $branch (ultra, or a tag) at $commit, newer than run $after, or $null
# when none appears. GitHub takes a few seconds to start one, so this asks up to $tries times.
function Find-Run([string]$branch, [string]$commit, [long]$after = 0, [int]$tries = 24) {
  for ($i = 0; $i -lt $tries; $i++) {
    $json = gh run list -R $Repo --workflow $workflow --event push --branch $branch --commit $commit --limit 5 --json databaseId,url,status,conclusion
    if ($LASTEXITCODE -eq 0 -and $json) {
      $run = @(($json -join "`n") | ConvertFrom-Json | Where-Object { $_.databaseId -gt $after })[0]
      if ($run) { return $run }
    }
    Start-Sleep -Seconds $pollSeconds
  }
  return $null
}

# The id of the workflow's newest run: runs that start later have higher ids.
function Get-NewestRunId {
  $json = gh run list -R $Repo --workflow $workflow --limit 1 --json databaseId
  if ($LASTEXITCODE -ne 0) { throw "Could not list the runs of $workflow on $Repo." }
  return [long](@(($json -join "`n") | ConvertFrom-Json)[0].databaseId ?? 0)
}

# A run of the workflow that has not finished, or $null. Each status is asked for on its own, so a
# re-run of an old run counts too.
function Get-UnfinishedRun {
  foreach ($status in 'in_progress', 'queued', 'waiting', 'pending', 'requested') {
    $json = gh run list -R $Repo --workflow $workflow --status $status --limit 1 --json databaseId,url,status
    if ($LASTEXITCODE -ne 0) { throw "Could not list the runs of $workflow on $Repo." }
    $run = @(($json -join "`n") | ConvertFrom-Json)[0]
    if ($run) { return $run }
  }
  return $null
}

# Whether the release the channel holds has $commit. Its commit comes from origin, so a release
# landing meanwhile, or a tag here that origin has since replaced, cannot mislead this.
function Test-Published([string]$commit) {
  $holds = Get-ChannelVersion
  if (-not $holds) { return $false }
  $null = Invoke-Checked 'git fetch' { git -C $root fetch -q origin ultra }
  $built = Get-RemoteTagCommit "ultra-v$holds"
  return [bool]($built -and (Test-Ancestor $commit $built))
}

function Get-Mode { Get-Content -LiteralPath (Join-Path $stageDir 'mode') -Raw }

# What Decide found changed: 'shipping', 'tooling', 'fleet' or 'nothing'.
function Get-Changed { Get-Content -LiteralPath (Join-Path $stageDir 'changed') -Raw }

function Get-RemoteUltra {
  $line = @(git -C $root ls-remote origin refs/heads/ultra)[0]
  if ($LASTEXITCODE -ne 0 -or -not $line) { throw 'Could not read ultra on origin.' }
  return ($line -split '\s+')[0]
}

function Watch-Run($run) {
  if ($run.status -eq 'completed') { return }
  Write-Host "Waiting for $($run.url) (usually 10 to 20 minutes; runs take turns)."
  gh run watch $run.databaseId -R $Repo --interval 30 | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "Could not watch $($run.url); it may still be running." }
}

# A push that changes what ships publishes itself: follow its run, or the run of a newer push that
# replaced it or came after it, which publishes $commit too. Pushes no tag: a tag's run would
# replace whichever run waits. $null when a run ended without publishing $commit because nothing
# that ships is missing any more (Decide runs again; the caller goes by its mode).
function Wait-ForPush([string]$commit) {
  $tip = $commit
  for ($round = 0; $round -lt 10; $round++) {
    $run = Find-Run 'ultra' $tip -tries 12
    if (-not $run) { throw "GitHub has started no release run for the push of $tip, and nothing was pushed. See https://github.com/$Repo/actions/workflows/$workflow." }
    Write-Host "The push of $tip publishes $commit by itself, so no tag is pushed."
    Watch-Run $run
    if (Test-Published $commit) { return $run }
    $newer = Get-RemoteUltra
    if ($newer -eq $tip) {
      Invoke-Decide | Out-Host
      if ((Get-Mode) -ne 'push') { return $null }
      throw "$($run.url) ended without publishing $commit. Re-run it (gh run rerun $($run.databaseId) -R $Repo) or push again: either publishes it."
    }
    $tip = $newer
  }
  throw "ultra keeps moving and $commit is not published yet: see https://github.com/$Repo/actions/workflows/$workflow."
}

# A commit whose push publishes nothing although the release tooling changed since the channel's
# release, so a build may differ: it gets a tag of its own, pushed only when no run can race it.
function Publish-ByTag($release) {
  $commit = $release.commit
  # The tags, read once. One already on this commit (another run of this, or the release that just
  # landed) is followed, not doubled.
  $refs = @(git -C $root ls-remote --tags origin 'refs/tags/ultra-v*')
  if ($LASTEXITCODE -ne 0) { throw 'Could not list the ultra-v tags on origin.' }
  $tagged = @($refs | ForEach-Object {
      $sha, $ref = $_ -split "`t"
      if ($sha -eq $commit) { $ref -replace '^refs/tags/' -replace '\^\{\}$' }
    } | Select-Object -Unique)
  if ($tagged) {
    $tag = $tagged[0]
    if ($Build) { throw "-Build does not apply: $tag is already on $commit." }
    if (Test-Published $commit) { return [pscustomobject]@{ url = "https://github.com/$Repo/releases/tag/$tag" } }
    Write-Host "$tag is already on $commit; following its run."
    $run = Find-Run $tag $commit
  } else {
    $package = $release.version -replace '-ultra\.\d+$'
    $highest = Get-HighestBuild $package (Get-ChannelVersion) $refs
    $number = if ($Build) { $Build } else { $highest + 1 }
    if ($number -le $highest) { throw "Build $number is no longer higher than $package-ultra.$highest. Nothing was pushed." }
    $tag = "ultra-v$package-ultra.$number"
    # Last, right before the push: ultra is still this commit, and no run is unfinished. The tag's
    # run then starts at once, where no newer run can replace it, and replaces no waiting run.
    if ((Get-RemoteUltra) -ne $commit) { throw "ultra on origin moved on from $commit, and its push publishes what ships. Nothing was pushed." }
    $busy = Get-UnfinishedRun
    if ($busy) { throw "A release run has not finished ($($busy.url)); a tag's run would wait behind it, where a newer run could replace it. Nothing was pushed; run this again once it has finished." }
    $floor = Get-NewestRunId
    Invoke-Checked 'Pushing the tag' { git -C $root push -q origin "$($commit):refs/tags/$tag" }
    Write-Host "Pushed $tag; the workflow builds, signs and publishes it."
    $run = Find-Run $tag $commit $floor
  }
  if (-not $run) { throw "No release run started for $tag; see https://github.com/$Repo/actions/workflows/$workflow." }
  Watch-Run $run
  if (-not (Test-Published $commit)) { throw "$($run.url) ended without publishing $commit. Re-run it: gh run rerun $($run.databaseId) -R $Repo" }
  return $run
}

# From a machine without the key: have the workflow publish origin/ultra, and confirm. A push that
# changes what ships publishes itself and is followed, never raced with a tag: a tag pushed while
# the push's run waits replaces that run, and one pushed while it runs loses (the run takes the next
# number above the tag, and the tag's run then fails, older than the channel). A tag is pushed only
# for a commit whose push publishes nothing although the release tooling changed.
function Invoke-ViaActions {
  $release = Read-StageFile 'release.json'
  $names = @(gh secret list -R $Repo --json name --jq '.[].name')
  if ($LASTEXITCODE -ne 0) { throw "Could not list the secrets of $Repo (the owner's gh sign-in can)." }
  $retired = @($retiredSecretNames | Where-Object { $names -contains $_ })
  if ($retired) {
    throw "Delete $($retired -join ' and ') from ${Repo}: the release workflow of older commits reads those names and would publish without the installer checks."
  }
  $missing = @($secretNames | Where-Object { $names -notcontains $_ })
  if ($missing) {
    throw "$($missing -join ' and ') not set on $Repo, so the workflow would publish nothing. Add the secrets (docs/ULTRA-RELEASES.md) or publish on the machine with the key."
  }
  $commit = $release.commit
  if (Test-Published $commit) {
    Write-Output "Nothing to publish: the channel's release already has $commit."
    return
  }
  Invoke-Decide
  $run = $null
  if ((Get-Mode) -eq 'push') {
    if ($Build) { throw "-Build does not apply: the push of $commit publishes it by itself, under the next number." }
    $run = Wait-ForPush $commit
  }
  if (-not $run) {
    if ((Get-Mode) -eq 'none') {
      Write-Output "Nothing to publish: nothing that ships, or builds it, changed since the channel's $(Get-ChannelVersion)."
      return
    }
    # The workflow rehearses a change of the fleet report so its test runs, but no installer holds it:
    # a release of this commit would be the installer the channel has, again.
    if ((Get-Changed) -eq 'fleet') {
      Write-Output "Nothing to publish: since the channel's $(Get-ChannelVersion) only the fleet report changed, which no installer holds."
      return
    }
    $run = Publish-ByTag $release
  }
  # A push's run takes its own build number, so this reports what the channel holds.
  $holds = Get-ChannelVersion
  Write-Output "Published $holds through the workflow"
  Write-Output "  release: https://github.com/$Repo/releases/tag/ultra-v$holds"
  Write-Output "  run: $($run.url)"
}

# Dot-sourced (scripts/test-publish-ultra-release.ps1): define the functions only.
if ($MyInvocation.InvocationName -eq '.') { return }

if (($Tag -or $Auto) -and ($Step -eq 'All' -or $ViaActions -or $Build)) { throw '-Tag and -Auto are for the workflow: one -Step at a time, with no -Build or -ViaActions.' }
if ($Tag -and $Auto) { throw 'A run publishes either a pushed tag (-Tag) or a push to ultra (-Auto).' }
if ($ViaActions -and $Step -ne 'All') { throw '-ViaActions runs the steps it needs itself.' }
if ($Step -ne 'All' -and -not $Stage) { throw "-Step $Step needs -Stage, the folder the steps share." }
$stageDir = if ($Stage) { $Stage } else { Join-Path ([IO.Path]::GetTempPath()) "slopbar-ultra-release-$PID" }
$null = New-Item -ItemType Directory -Force $stageDir

switch ($Step) {
  'Decide' { Invoke-Decide }
  'Check' { Invoke-Check }
  'Build' { Invoke-Build }
  'Sign' { Invoke-Sign }
  'Publish' { Invoke-Publish }
  'All' {
    Invoke-Check
    if ($ViaActions) { Invoke-ViaActions; break }
    if (-not (Test-Path (Join-Path $KeyDir 'updater.key'))) {
      throw "This machine has no updater key in $KeyDir. Publish through the workflow instead: pwsh scripts/publish-ultra-release.ps1 -ViaActions"
    }
    Invoke-Build
    Invoke-Sign
    Invoke-Publish
  }
}
# Every native exit code that matters was checked. Without this, a caller that exits with
# $LASTEXITCODE (as GitHub's pwsh steps do) would take the last one, such as `git diff --quiet`'s 1.
exit 0
