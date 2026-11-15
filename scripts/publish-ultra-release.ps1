#Requires -Version 7
<#
Publishes a SlopBar Ultra release and moves the update channel (release ultra-latest) to it. A run
on a machine and the release workflow (.github/workflows/ultra-release.yml) run the same steps of
this script, so they build, check, sign and publish alike. See docs/ULTRA-RELEASES.md.

Normally nobody runs it: every push to ultra that changes what ships ($shippingPaths) is published
by the workflow. By hand:
  pwsh scripts/publish-ultra-release.ps1              # on the machine with the updater key
  pwsh scripts/publish-ultra-release.ps1 -Build 7     # a given build number (higher than any before)
  pwsh scripts/publish-ultra-release.ps1 -ViaActions  # any machine: pushes a tag; the workflow publishes it
The workflow, one step per call, sharing the -Stage folder:
  -Step Decide -Before <sha>             on a push to ultra: publish, rehearse, or nothing (mode file)
  -Step Check -Auto | -Tag <tag>         a push to ultra, or a pushed tag
  -Step Build|Sign|Publish [-Tag <tag>]

Decide   Publish when anything under $shippingPaths changed since the channel's release; rehearse
         (Check and Build only) when only the release tooling changed in this push; else nothing.
Check    The checkout is clean. By hand: it is exactly origin/ultra. On a push: the pushed commit, on
         ultra. Both take the next free build number, above every ultra-v tag and the channel. On a
         tag: the tagged commit, on ultra, under the tag's version. Always: the version is newer than
         the channel's, and the commit contains the one the channel's release was built from.
Build    Unit tests, then the installer with ultra-updates, the installer hook that sets a running
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
# What ships: everything the installer is built from. A change here is published.
$shippingPaths = @('src', 'src-tauri', 'package.json', 'pnpm-lock.yaml')
# The release tooling: a change here alone rehearses the build.
$toolingPaths = @('.github/workflows/ultra-release.yml', 'scripts/publish-ultra-release.ps1', 'scripts/test-publish-ultra-release.ps1')
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
function Get-HighestBuild([string]$package, [string]$current) {
  $refs = @(git -C $root ls-remote --tags origin "refs/tags/ultra-v$package-ultra.*")
  if ($LASTEXITCODE -ne 0) { throw 'Could not list the ultra-v tags on origin.' }
  $taken = @($refs | ForEach-Object { if ($_ -match "refs/tags/ultra-v$([regex]::Escape($package))-ultra\.(\d+)$") { [int]$Matches[1] } })
  if ($current -match '^(\d+\.\d+\.\d+)-ultra\.(\d+)$' -and $Matches[1] -eq $package) { $taken += [int]$Matches[2] }
  return [int](($taken | Measure-Object -Maximum).Maximum ?? 0)
}

# 0. On a push to ultra (the workflow): publish, rehearse or nothing. Compared with the channel's
# release, not the previous push, so the next run catches up whatever an earlier one did not publish.
function Invoke-Decide {
  Invoke-Checked 'git fetch' { git -C $root fetch -q origin ultra --tags }
  $head = (git -C $root rev-parse HEAD).Trim()
  $current = Get-ChannelVersion
  $built = if ($current) { Get-TagCommit "ultra-v$current" }
  if (-not $built -or (Test-Changed $built $head $shippingPaths)) {
    $mode = 'push'
    $why = if ($built) { "what ships changed since $current" } else { 'no release yet' }
  } else {
    # The tooling alone changed in this push (or, without a usable previous commit, since the release).
    $since = if ($Before -match '^[0-9a-f]{40}$' -and $Before -notmatch '^0+$' -and (Test-Ancestor $Before $head)) { $Before } else { $built }
    $mode = if (Test-Changed $since $head $toolingPaths) { 'rehearsal' } else { 'none' }
    $why = if ($mode -eq 'rehearsal') { 'only the release tooling changed' } else { "nothing that ships changed since $current" }
  }
  Set-Content -LiteralPath (Join-Path $stageDir 'mode') -Value $mode -NoNewline
  Write-Output "Decision for $($head): $mode ($why)."
}

# 1. Source and version.
function Invoke-Check {
  $conf = Read-Config
  $package = $conf.version
  # origin/ultra and every tag, the channel's included (a fresh fetch, whatever the checkout fetched).
  Invoke-Checked 'git fetch' { git -C $root fetch -q origin ultra --tags }
  if (git -C $root status --porcelain) { throw 'The checkout has uncommitted changes.' }
  $head = (git -C $root rev-parse HEAD).Trim()
  $current = Get-ChannelVersion
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
  Push-Location $root
  try {
    Invoke-Checked 'pnpm install' { & $pnpm[0] @pnpmArgs install --frozen-lockfile }
    Invoke-Checked 'Unit tests' { cargo test --locked --no-default-features --lib --manifest-path src-tauri/Cargo.toml }
    Invoke-Checked 'Build' { & $pnpm[0] @pnpmArgs tauri build --features ultra-updates --config $override }
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

# From a machine without the key: push the tag, let the workflow publish it, and confirm.
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
  Invoke-Checked 'Pushing the tag' { git -C $root push -q origin "$($release.commit):refs/tags/$($release.tag)" }
  Write-Output "Pushed $($release.tag); the workflow builds, signs and publishes it."
  $run = $null
  for ($i = 0; $i -lt 24 -and -not $run; $i++) {
    Start-Sleep -Seconds 5
    $json = gh run list -R $Repo --workflow $workflow --event push --branch $release.tag --limit 1 --json databaseId,url
    if ($LASTEXITCODE -eq 0 -and $json) { $run = @($json | ConvertFrom-Json)[0] }
  }
  if (-not $run) { throw "No workflow run started for $($release.tag); see https://github.com/$Repo/actions/workflows/$workflow." }
  Write-Output "Waiting for $($run.url) (usually 10 to 20 minutes; runs take turns)."
  gh run watch $run.databaseId -R $Repo --exit-status --interval 30 | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "The workflow run failed: $($run.url)" }
  $back = Save-Channel
  $holds = if ($back) { (Get-Content -LiteralPath $back -Raw | ConvertFrom-Json).version }
  if ($holds -ne $release.version) { throw "The run finished, but the channel holds $($holds ?? 'nothing'), not $($release.version): $($run.url)" }
  Write-Output "Published $($release.version) through the workflow"
  Write-Output "  release: https://github.com/$Repo/releases/tag/$($release.tag)"
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
