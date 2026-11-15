# SlopBar Ultra releases and updates

SlopBar Ultra ships its own releases from this fork (`layibabalola/slopbar-ultra`) and updates
itself from them only. This is fork-only: it lives on the `ultra` branch and never goes to
`claude/subscription-value` or upstream pull requests.

## How it works

| Build | How it is made | Updates |
| --- | --- | --- |
| Local build (`scripts/install-local-build.ps1`) | `cargo build --release --features tauri/custom-protocol` | Manual only ("Manual updates only in this build") |
| Ultra release | `.github/workflows/ultra-release.yml`, by itself, on every push to `ultra` that changes what ships (or `scripts/publish-ultra-release.ps1` by hand) | Automatic, from this fork only |
| Upstream SlopBar | upstream's `release.yml` with `upstream-updates` | From `thelioo/slopbar` |

- **Feature `ultra-updates`** (`src-tauri/Cargo.toml`) turns on automatic updates for Ultra
  release builds only. Local and dev builds don't enable it, so they stay manual.
- **Update channel.** Ultra installs read
  `https://github.com/layibabalola/slopbar-ultra/releases/download/ultra-latest/latest.json`.
  That is a fixed release named `ultra-latest`, not `releases/latest`, so no other release on this
  fork (such as "SlopBar Enhanced") can become an Ultra update. Each Ultra release moves the
  channel to itself.
- **Fork key.** Updates must be signed with the fork's own updater key (key id
  `593D8A8B75A9A204`). Its public key is in `tauri.conf.json` and `src-tauri/src/ultra_release.rs`.
  The private key is only on the owner's machine (BACHELOR) and in the fork's `ULTRA_UPDATER_KEY`
  secrets.
- **An Ultra install never updates to upstream SlopBar**, because of three separate checks:
  1. `updates.rs` pins the Ultra channel URL and fork key in code under `ultra-updates`, so a
     config change merged from upstream can't point the app back at `thelioo/slopbar`.
  2. Upstream releases are signed with upstream's key, so their signature check fails.
  3. The app only accepts versions shaped `<x.y.z>-ultra.<n>` that are newer than the installed
     one. Upstream's `0.2.N` versions are refused.

  A unit test (`ultra_release::tests`, which runs in CI and in every release build) fails if
  `tauri.conf.json` stops pointing at the Ultra channel and key.

### Version scheme

The package version stays as it is (`tauri.conf.json` `version`, currently `0.1.0`). An Ultra
release adds a build suffix: version `0.1.0-ultra.<n>`, tag `ultra-v0.1.0-ultra.<n>`, release name
`SlopBar Ultra 0.1.0-ultra.<n>`. `<n>` goes up with every release: each one takes the next free
number, above every `ultra-v` tag and the channel's. A release must also be newer than the
channel's, and its commit must contain the one the channel's release was built from, so a higher
number can never ship older code.

## Publishing a release

Releases publish themselves. Every push to `ultra` that changes what ships (anything under `src/`,
`src-tauri/`, `package.json` or `pnpm-lock.yaml`) is published by `.github/workflows/ultra-release.yml`
as the next `0.1.0-ultra.<n>`, and installed copies take it at their next check. A push that changes
only docs, the roadmap or the README publishes nothing. After merging into `ultra` and pushing,
nothing is left to do. To watch: `gh run list -R layibabalola/slopbar-ultra --workflow ultra-release.yml`.

Installed copies pick the release up at their next check: about 20 seconds after SlopBar starts,
then every 15 minutes (`ultra_release::CHECK_EVERY`; upstream builds keep 6 hours). So a fix reaches
every machine about 10 minutes for the build plus at most 15 for the check after the push. To have
every machine install now, run `pwsh scripts/fleet-slopbar.ps1 -UpdateNow` (below).

The workflow runs the steps of `scripts/publish-ultra-release.ps1`. The script also publishes by
hand, for example while Actions is down:

- **On the machine with the key** (`%USERPROFILE%\.slopbar-ultra\updater.key`, BACHELOR), the
  script builds, signs and publishes on that machine.
- **From any machine**, `-ViaActions` has the workflow publish `origin/ultra`. When the push of
  `ultra` publishes the commit anyway, it follows that run instead of racing it with a tag.

By hand, run it in a clean checkout whose `HEAD` is the pushed `origin/ultra`. A detached worktree
keeps the working `ultra` checkout free. Make it once per machine with
`git -C C:\code\slopbar worktree add --detach C:\code\slopbar-worktrees\ultra-release origin/ultra`,
then:

```powershell
git -C C:\code\slopbar-worktrees\ultra-release fetch -q origin
git -C C:\code\slopbar-worktrees\ultra-release checkout -q --detach origin/ultra
# On the machine with the key:
pwsh C:\code\slopbar-worktrees\ultra-release\scripts\publish-ultra-release.ps1
# On any machine:
pwsh C:\code\slopbar-worktrees\ultra-release\scripts\publish-ultra-release.ps1 -ViaActions
```

On a machine without the key, the first form stops before building and suggests `-ViaActions`.

The steps:

0. **Decide** (the workflow, on a push to `ultra`). It publishes when anything that ships changed
   since the release the channel holds. It rehearses (Check and Build, nothing kept) when only the
   release tooling changed in this push: the workflow, the script or its test. Otherwise it does
   nothing.
1. **Check.** The checkout is clean. By hand it must be exactly `origin/ultra`. On a push, it is the
   pushed commit, which must be on `ultra`. In both cases the version takes the next free build
   number, above every `ultra-v` tag and the channel's. On a tag, it is the tagged commit, on
   `ultra`, under the tag's version. Always: the version must be newer than the channel's, and the
   commit must contain the channel's.
2. **Build.** Unit tests, then `tauri build --features ultra-updates --config <override>`. The
   override sets the version, leaves the updater signature to step 3, and adds the installer hook
   for `slopbar-mcp.exe` (below). No signing variable is in the build's environment. The
   executable must report a clean build of this commit, and the installer must carry
   `slopbar-mcp.exe` with the hook.
3. **Sign.** `tauri signer sign`. Only the signer's process gets the password, and in the workflow
   the key as well, through its environment. Neither is printed or passed as an argument. The
   signature's key id must be the one in `tauri.conf.json`.
4. **Publish.** Creates `ultra-v<version>` with exactly the installer, its `.sig` and `latest.json`,
   with `slopbar-mcp.exe`'s SHA-256 in the notes. Downloads the three files back and compares them,
   then moves `ultra-latest` and reads it back. The channel is checked again before publishing and
   again before moving, so it never goes back if another release moved it in the meantime.

`-ViaActions` runs Check on your machine and makes sure both secrets exist. Then:

- **Already published.** If the channel's release already has the commit, it stops: there is nothing
  to publish.
- **The push publishes it.** If what ships changed, the push's own run publishes the commit. It
  follows that run and pushes no tag. A newer push may replace that run while it waits, or come
  after it. The newer push's run publishes this commit too, so it follows that run instead.
  - If a newer release already has everything that ships, it stops: nothing is left to publish.
  - If GitHub started no run, it stops with nothing pushed. Look at the workflow's runs.
  - If the run ended without publishing, it stops with nothing pushed and names the run to re-run
    (`gh run rerun <id>`). A tag there would race the re-run or a newer push's run.
- **Nothing that ships changed, but the release tooling did** (since the channel's release), so a
  build may differ. If a tag is already on the commit (another run of `-ViaActions`), it follows
  that tag's run. Otherwise, right before tagging, it checks that `ultra` on origin is still the
  commit and that no release run is unfinished. Then it pushes the tag `ultra-v<version>` under the
  next free number. With nothing else running, the tag's run starts at once, where no newer run can
  replace it.
- **Only documentation changed.** It publishes nothing: the installer would be the same.

A run usually takes 10 to 20 minutes. At the end it confirms that the channel holds a release with
the commit; a push's run picks its own build number. `-Build` only applies to a new tag.

Don't push a tag by hand (`git push origin origin/ultra:refs/tags/ultra-v0.1.0-ultra.<n>`) for a
commit whose push publishes it. Pushed while the push's run is running, the tag loses: the run takes
the next number above the tag, and the tag's run then fails as older than the channel. Nothing is
published twice, but a red run and a tag without a release are left behind. Pushed while the push's
run is still waiting, the tag's run replaces it.

### Order, and one publisher per release

- **One run at a time.** The workflow's runs take turns and are never cancelled, so each takes its
  build number after the previous one has finished. Two quick pushes publish one after the other
  and never collide. While one run works, a newer run replaces an older waiting one. A newer push's
  run publishes both pushes, because every run compares with the channel's release, not with the
  push before. A tag's run publishes only its tag, so `-ViaActions` pushes one only when no run is
  unfinished.
- **Tags.** A push's release and its tag appear together. The workflow makes them, and its token
  starts no further runs. The script on the key's machine also makes its tag together with the
  release, so the workflow finds the release on that tag and stands down. Only a tag pushed on its
  own (`-ViaActions`, for a change of the release tooling alone) arrives without a release, and the
  workflow publishes it.
- **Backstops.** `gh release create` refuses a tag that already has a release. On a pushed tag the
  workflow also passes `--verify-tag`, so it never makes one. Every publisher refuses a tag that
  appeared on `origin` while it was building, and checks the channel right before moving it.

If a run fails, the next push that changes what ships publishes everything pending, and so does
re-running the failed run. A run on a tag that already has its release only stands down.

### The workflow

`.github/workflows/ultra-release.yml` has three jobs:

- `gate` takes seconds. On a push it runs Decide; on a tag, the stand-down checks above. It fails
  while a secret under a retired name exists (below).
- `build` runs on Windows with no secrets and a read-only token, because the build runs third-party
  build scripts. It runs Check and Build, then the publisher's offline tests, and keeps the
  installer for `publish` (except in a rehearsal).
- `publish` runs on a fresh Windows runner. It installs the signer from the lockfile, then runs
  Sign, the only step that gets the secrets, and Publish, the only step whose token can write.

### What still differs between the workflow and a run by hand

- **Which commit.** By hand, the checkout, which must be exactly `origin/ultra`. The workflow
  releases the pushed or tagged commit, which must be on `ultra` and contain the channel's commit.
- **The key.** The script reads the key and password files. The workflow gets both from the
  secrets. Either way only the signer receives them, and the signature's key id is checked the same
  way.
- **Isolation.** The workflow builds without the key and signs on a different runner. The script
  builds as the user whose profile holds the key files.
- **Toolchain.** The runner has its own Rust, Node and Visual Studio versions, so two builds of one
  commit are not byte-identical. Both pass the same checks.
- **Temp folder.** The runner's `TEMP` is spelled with an 8.3 short name (`C:\Users\RUNNER~1\…`).
  The unit tests run there as in the repository's Tests workflow, and the `pins` tests need such a
  spelling: in CI they fail rather than skip without one.

### Agents' MCP server (`slopbar-mcp.exe`)

The installer carries `slopbar-mcp.exe` beside `slopbar.exe` (Tauri bundles every binary of the
crate). Agents run it for their whole session, though, and a running executable can't be written,
so a quiet update used to skip it and leave the old build behind. Windows does allow renaming it.
The build's `--config` override adds the hook `src-tauri/nsis/ultra-hooks.nsh`, so release
installers first rename the file to `slopbar-mcp.old-<time>-<n>.exe` and then write the new one.
Running servers carry on from the old copy, and new agent sessions start the new one. At every
start the app deletes old copies that nothing runs any more (`src-tauri/src/mcp_update.rs`), and
the uninstaller removes them with the app. Both ways of publishing refuse an installer without the
server and the hook.

The release notes give the server's SHA-256. `get_app_info` (door) reports `mcp.sha256` and
`mcp.set_aside`, the number of old copies still running agents' servers.

**After moving machines to releases, don't run `scripts/install-local-build.ps1` on them.** A
local build reports version `0.1.0`, which SemVer ranks above every `0.1.0-ultra.<n>`, and it
can't update itself. So that machine stops taking releases until a release build is installed over
it again.

### The GitHub secrets

The workflow signs with two secrets. To set or replace them, run this on the machine with the key. The keypair was generated on 2026-10-22 and lives outside the
repository, readable only by your Windows user:

- `%USERPROFILE%\.slopbar-ultra\updater.key`: private key (encrypted with the password)
- `%USERPROFILE%\.slopbar-ultra\updater.key.password`: its password
- `%USERPROFILE%\.slopbar-ultra\updater.key.pub`: public key (already committed)

Back up the folder somewhere private, such as a password manager. **If the key is lost, installed
copies can't verify any future update.** You would have to reinstall them by hand with a build
that carries a new key.

From PowerShell, without showing either value:

```powershell
Get-Content -Raw "$env:USERPROFILE\.slopbar-ultra\updater.key" | gh secret set ULTRA_UPDATER_KEY -R layibabalola/slopbar-ultra
Get-Content -Raw "$env:USERPROFILE\.slopbar-ultra\updater.key.password" | gh secret set ULTRA_UPDATER_KEY_PASSWORD -R layibabalola/slopbar-ultra
gh secret list -R layibabalola/slopbar-ultra
```

Both must be set: with one alone, tag runs fail at `gate`. The script trims the trailing line end
that `Get-Content -Raw` leaves, as it does for the files.

The names matter:

- They are `ULTRA_`-prefixed. Upstream's `release.yml` is also on this fork and reads
  `TAURI_SIGNING_PRIVATE_KEY`. With that name unset, it can never sign a release with the fork's
  key, so it can't produce anything an Ultra install would accept.
- Never create `ULTRA_TAURI_SIGNING_PRIVATE_KEY` or `ULTRA_TAURI_SIGNING_PRIVATE_KEY_PASSWORD`,
  the names these docs used to give. A tag runs the workflow file of the tagged commit, and in
  every commit before "Publish Ultra releases from any machine" (2026-11-14) that file reads those
  names. Given a key, it would publish through `tauri-action`, without the installer hook or any of
  the checks above, whenever a commit that old is tagged or one of its runs is re-run. Under the
  new names, those old workflow files never see a key. While either old name exists, the workflow
  fails at `gate` and `-ViaActions` refuses.

## Installing on another machine

1. Download `SlopBar_<version>_x64-setup.exe` from the newest `ultra-v…` release:
   `gh release download ultra-v0.1.0-ultra.1 -R layibabalola/slopbar-ultra -p "*-setup.exe"`,
   or use the release page.
2. Run it. The installer is per-user and needs no admin rights. It's unsigned (see below), so
   SmartScreen may say "Windows protected your PC": choose **More info → Run anyway**.
3. Sign in to accounts in SlopBar Ultra as usual. On a fresh install, "Update automatically" is
   on by default (Settings → Updates). The app checks about 20 seconds after launch and then every
   15 minutes, and installs Ultra releases quietly.

On a machine that already runs a local build (`scripts/install-local-build.ps1`), installing a
release over it makes updates possible. The saved settings keep "Update automatically" off,
though, so turn it on under Settings → Updates. Running the local install script again later puts
back a manual build.

## The fleet

`scripts/fleet-slopbar.ps1` reports every machine's SlopBar Ultra against the channel, and
`-UpdateNow` makes each machine that is behind install the channel's release now, then waits until it
reports the new version:

```powershell
pwsh scripts/fleet-slopbar.ps1
pwsh scripts/fleet-slopbar.ps1 -UpdateNow
```

- **Machines:** this one, plus each stanza with a `HostName` in the managed block of `~/.ssh/config`
  (between `# >>> fleet-ssh managed block` and `# <<< fleet-ssh managed block <<<`). No host names
  or addresses are in the repository.
- **How it asks:** each machine's own `slopbar-mcp.exe --call` (`get_app_info`, `get_settings`,
  `install_update`), all machines at once. Over ssh the command is
  `cmd /d /c if exist <door> <door> --call <tool>`, which works under a PowerShell or cmd remote
  shell as long as the profile path has no spaces. ssh runs through `Start-Process` with its output
  in files, a timeout and a tree kill, because on some machines an `ssh.exe` whose output is piped
  never exits.
- **What it reports:** version, "Update automatically", and current, behind or not installed. A
  machine without SlopBar is "not installed", not an error.
- **Updating:** `install_update` replies at once and installs in the background. The app drops the
  install's error, and it puts an update off while a sign-in is open. So a machine still answering
  on the old version after 90 seconds is asked again, up to three times in all.
- **Problems (exit code 1):**
  - a local build (a version without `-ultra.`, which takes no releases),
  - a build without updates,
  - "Update automatically" off,
  - a machine that does not answer,
  - an update that does not arrive within 5 minutes.

## Optional: Authenticode signing (paid, not purchased)

The updater signature above proves that an update came from this fork. It doesn't sign the `.exe`
for Windows, so SmartScreen warns on first download until the file gains reputation. To remove the
warning, the installer needs an Authenticode signature. Options, none bought:

- **Azure Trusted Signing** (Microsoft's managed signing, about US$10/month at the time of writing;
  identity validation required). Integrates with GitHub Actions through
  `azure/trusted-signing-action` or Tauri's `bundle.windows.signCommand`.
- **OV or EV code-signing certificate** from a CA (roughly US$200–600/year; since 2023 the key
  must live on a hardware token or cloud HSM). EV used to get SmartScreen reputation instantly,
  but Microsoft no longer guarantees that.

Whichever you choose, wire it in through `bundle.windows.signCommand` (in `tauri.conf.json` or the
build override), so the installer is signed during Build, before the updater signature covers it.
Both ways of publishing then need its credentials: further `ULTRA_*` secrets for the workflow, and
the same on the key's machine. The updater key stays as it is.

## Verifying locally

```powershell
cargo test --locked --no-default-features --lib --manifest-path src-tauri/Cargo.toml ultra_release
pwsh scripts/test-publish-ultra-release.ps1
```

The second tests the publisher offline, against synthetic repositories and a stand-in for `gh`.
After `pnpm install` it also signs with a throwaway key.

Don't set the signing variables in a shell by hand: they pass to every process it starts. To sign,
use `scripts/publish-ultra-release.ps1`, which gives the password to the signer's process alone.

## Rolling back a bad release

Installed copies move only forward, so a bad release is replaced by a newer one, never by
rewinding the channel:

1. Turn off "Update automatically" on machines that haven't taken it yet (door `update_settings`).
2. On a machine that took it, put the previous executables back from
   `%LOCALAPPDATA%\SlopBar-Rollback\<time>` (the remote swap keeps one) and restart through the door.
3. Revert the fix on `ultra`, push, and publish again. The script gives it the next build number.
