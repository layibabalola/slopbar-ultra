# Sharing in-use status through cloud storage

SlopBar can show which accounts are in use on the user's other computers. Each SlopBar keeps one
small presence record per machine. The record holds the machine id, the computer name, when it
was written, and hashed account ids with where each account is signed in. It never holds emails,
tokens or organization names. See `src-tauri/src/presence.rs`.

Records go through the `PresenceStore` trait (`list`, `write`, `clear`). The reader, the staleness
rules and the UI only see that trait.

| Backend | Status | Needs |
| --- | --- | --- |
| `SyncedFolder`: any folder a sync client mirrors | Shipped | Nothing. Settings > Data detects OneDrive, Dropbox, Google Drive for desktop, iCloud Drive and Box, or takes a custom folder. |
| `Rclone`: any remote the user set up with `rclone config`, or Dropbox through the guided setup | Shipped | rclone, which the guided setup downloads when it is missing. **No app registration.** |
| `CloudApi(Dropbox)` | Optional stub, reports `not_configured` | A Dropbox app registered by the owner |
| `CloudApi(GoogleDrive)` | Optional stub, reports `not_configured` | A Google Cloud OAuth client registered by the owner |
| `CloudApi(OneDrive)` | Optional stub, reports `not_configured` | A Microsoft Entra app registered by the owner |

Machines without a sync client should use rclone. The direct API backends below are
**optional**: build them only if a sign-in inside SlopBar itself becomes worth registering apps
for. Every API backend would store records in an **app-private folder only**, so SlopBar never
asks for access to the user's own files.

The guided Dropbox setup does not meet that bar, and says so on its button: it signs in through
rclone's own registered Dropbox client, whose access covers the user's whole Dropbox. rclone, not
SlopBar, holds that token, and SlopBar only ever touches `SlopBar/presence`. A Dropbox app
registered for SlopBar with **App folder** access (`CloudApi(Dropbox)` below) would narrow it to
SlopBar's own folder; that needs the owner to register the app.

Google Docs is not a target. Presence records are JSON files, and Google Drive covers Docs users.

## rclone: any cloud, no app registration (`src-tauri/src/rclone.rs`)

The user connects their cloud once: Dropbox with SlopBar's guided setup (below), or any cloud
outside SlopBar with `rclone config`. That covers Dropbox, Google Drive, OneDrive, Box, pCloud,
MEGA, S3, WebDAV, SFTP and the other rclone backends. rclone keeps its own config and tokens.

- **Choosing a remote.** Settings > Data lists the remotes from `rclone listremotes --long` as
  "Dropbox (via rclone)" and so on, next to the detected sync folders. That command prints only
  names and backend types. SlopBar never runs `rclone config dump` or `config show`, and never
  reads `rclone.conf`, so it never sees a token.
- **When rclone is missing.** Settings offers the guided Dropbox setup, and a one-line hint for
  other clouds: "Install rclone and run `rclone config` to connect any cloud without a sync app."
- **Where rclone is found.** SlopBar's own copy (`%LOCALAPPDATA%\SlopBar\rclone\rclone.exe`, from
  the guided setup), then `%LOCALAPPDATA%\Microsoft\WinGet\Links`, then `PATH` (last: folders on
  `PATH` can be writable by anyone). All of them read the same rclone config.
- **Calls.** SlopBar runs only these, and only on `<remote>:SlopBar/presence` and, with the account
  roster on, `<remote>:SlopBar/roster`:
  - `lsjson --files-only --no-mimetype` to list records (with `--hash` on Dropbox and Google
    Drive, below);
  - `cat <file>` to read each record a refresh needs (below), and on Dropbox and Google Drive
    `cat --files-from-raw - --separator <random> <folder>` to read up to 16 of them in one call
    (below);
  - `rcat <id>.json` to write this machine's record in one step: rclone replaces the file whole
    (a cloud swaps in the finished upload; local, SFTP and FTP remotes get a partial file renamed
    into place, rclone v1.63 and later). Uploading `<id>.tmp` and then `moveto <id>.json`, as
    earlier versions did, left about a second with no file at all, because rclone deletes an
    existing file before it moves another onto its name; another machine reading then skipped
    this one for a refresh.

  A read that still misses a record (an older SlopBar writing the old way, a failed listing)
  keeps that machine's last record while it is live by its own time (`presence::Recall`).

  A refresh does not download this machine's own record (`PresenceStore::others`): every reader
  leaves out the record of this machine's id, and only a forgery would put another machine's
  record under its name. On Dropbox and Google Drive the listing adds `--hash`, as the roster's
  does (below), and a record listed with the hash and size of the copy the last read kept (kept
  only when its bytes had the hash it was listed with; in memory, at most 64 records of up to
  16 KiB) is not downloaded again. A running machine rewrites its record at every refresh, so
  that saves the records of machines asleep, shut down or retired; the running machines' records
  are due at every read. When several are due there, one call downloads up to 16 of them:
  `cat --files-from-raw -` with their names on its input (only names SlopBar writes,
  `<id>.json`, capitals too), which prints each file followed by a separator random for each call
  (rclone 1.63 or later), by name on Dropbox (checked with rclone 1.74.4). A piece of that output
  is taken only for the record whose listed size and hash it has, so one that changed since the
  listing, or holds the separator, is downloaded alone. When the call fails having given some,
  the records it did not give are not read that time, and `Recall` keeps their machines while
  they are live; so are all of a call's records when it is killed at 20 seconds, whatever it
  printed, and the next read then asks for half as many in a call as that one did (down to one at
  a time). When it fails having printed nothing, its records are downloaded alone, as are those
  of the calls it would have made after it in that read; if that works, the call was refused (an
  rclone older than 1.63 refuses `--separator`), and after two reads in a row like that none is
  made (one such read can be the network's moment; a read with a call rclone took, one that
  printed something, ended well or ran out of time, starts the count again). Both hold until an
  hour after the last read that ran out of time or was refused, or until SlopBar restarts; then
  the call is made in full again. A filter set in rclone's environment (`RCLONE_MAX_SIZE`…)
  refuses the batched call and a `cat` of one file alike, so presence over rclone reads nothing
  there, with or without batching. Output is not limited in size, as a `cat` of one file is not:
  a file that grows after the listing is read until rclone ends or is killed (rclone refuses
  `--max-size` beside `--files-from-raw`). Modelled for a refresh every 5 minutes
  (`rclone::tests::an_hour_of_presence_exchanges_over_rclone_starts_fewer_processes`), presence
  processes an hour go from 120 to 36 beside five running machines and two asleep ones (84 with
  each record downloaded alone), 60 to 36 beside two running ones, and 60 to 24 beside two asleep
  ones. The first read after SlopBar starts downloads every other record. A synced folder reads
  every file, which costs no process.

  The account roster (`SlopBar/roster`, `roster::Syncer`) uses `lsjson`, `cat <file>` and `rcat`
  as above (never the batched `cat`), and `deletefile <id>.roster` when the roster is turned off. On Dropbox and Google Drive its listing
  adds `--hash`: those clouds list a content hash this computer can check (Dropbox's, or SHA-256)
  with no extra request (checked on Dropbox with rclone v1.74.4: the same two requests with or
  without it; rclone 1.56 or later on Dropbox and 1.65 or later on Google Drive, as older ones list
  no hash this computer counts and so skip nothing).
  Other remotes are not asked: OneDrive lists only a quickxor, which other bytes can
  be made to match, Box only a SHA-1, and elsewhere rclone would compute hashes by reading every
  file. A roster listed with the hash and size of the copy the last read kept (kept only when its
  bytes had the hash it was listed with) is not downloaded again, at any pace, so a read costs the
  listing and one `cat` per roster that changed; a forced read (`sync_roster`) downloads every
  file. Modelled for a machine with two others, refreshing every 5 minutes
  (`rclone::tests::an_hour_of_roster_reads_over_rclone_starts_fewer_processes`), roster processes
  an hour go from 24 to 6 when nothing changes, 28 to 18 when readings change all along, 240 to 60
  with Settings open on Accounts, and 48 to 24 while another machine runs Sign in all.

  A remote's kind, which sets the proxy each call goes through, comes from `listremotes --long`
  and is kept 30 minutes. A listing, upload or deletion that fails forgets it, so a remote pointed
  at another cloud is looked up again at its first failed call, or after 30 minutes at most.

  A clean quit rewrites this machine's record with no accounts.
- **Each call.** rclone runs with no window, `--ask-password=false` (it never waits on an
  encrypted config's prompt), `--contimeout 10s --timeout 15s`, and a hard 20-second kill (a
  batched `cat` has the same 20 seconds for all its files). Calls
  run off the async runtime. rclone's stderr is discarded and its output is never logged.
- **The setting.** `settings.json` stores only the remote's name, `presence_remote`. A chosen
  remote takes precedence over `presence_folder`. A missing rclone means no sharing; it never
  falls back to a folder.
- **Tests.** Unit tests use a fake rclone, an injected `Runner`. Run the live check, which uses
  only `SlopBar/presence` and removes its synthetic record afterwards, with:
  `SLOPBAR_RCLONE_LIVE=<remote> cargo test --lib rclone_live -- --ignored`

### Guided Dropbox setup (`src-tauri/src/sharing_setup.rs`)

**Set up** in Settings → Data → Other computers (shown while no sharing location is chosen, and in
the getting-started guide), or the `setup_sharing` agent tool, connects a computer in one go:

1. **rclone.** An installed rclone of v1.60 or later is used as it is. Otherwise SlopBar downloads
   one fixed release (v1.76.0) from downloads.rclone.org, or its GitHub release if that site is
   down or broken, and takes it only when the zip's SHA-256 matches the one pinned in the code (as
   rclone's `SHA256SUMS` and winget's reviewed manifest publish it). It unpacks only `rclone.exe`
   with Windows' `tar.exe` (PowerShell's `Expand-Archive` where tar is missing), checks that it runs
   and reports that version, then moves it into SlopBar's folder. The download shows its progress,
   fails after 30 seconds without data or 10 minutes in all, and Cancel stops it; unpacking and the
   version check are killed after 3 minutes and 20 seconds.
   - The download follows redirects (GitHub's release links always redirect) only over HTTPS, at
     most five, and only to downloads.rclone.org, github.com and GitHub's `*.githubusercontent.com`
     file hosts. Like all of SlopBar's own requests (`src-tauri/src/net.rs`), it trusts what
     Windows trusts, through Windows' own certificate checks, so a network that inspects HTTPS works
     once its certificate is installed in Windows, and a certificate Windows distrusts is refused.
     It uses the proxy Windows would use, proxy scripts included, with HTTPS_PROXY first, and never
     goes around one it can't use (`proxy_unsupported`, `proxy_script`, `proxy_environment`).
   - A failed download says why, for each site tried (Settings and `get_sharing_setup`'s
     `reasons`): the host and a code such as `untrusted`, `distrusted`, `page` (a web page in the
     zip's place, as a web filter answers), `redirected` (with the host it led to), `status`,
     `proxy_auth` or `reset`, never a link. The GitHub release is tried only after the first site
     was down or broken (404, 410, 429 or 5xx, its CDN's error page included, or a broken or wrong
     zip): after a refusal that may be on purpose, the download ends there, so a company that
     blocks rclone is not worked around. A filter that answers a block with one of those statuses
     lets the GitHub release be tried, through the same filter.
   - When Windows or security software refuses to run rclone (an AppLocker, Application Control or
     Smart App Control policy, or antivirus taking the file), the setup stops with
     `rclone_blocked`. That includes an installed rclone: SlopBar never downloads a copy of its own
     to run from another folder in place of one a policy stopped. An installed rclone that fails
     otherwise ends the setup with `rclone_broken` (repair or remove it); only a missing one, one
     older than v1.60, or SlopBar's own broken copy is replaced by the download.
2. **Dropbox.** A remote of that name (the one sharing already uses, else `dropbox`) that reaches
   its storage is used as it is. Otherwise rclone signs in with its own flow:
   `rclone config create <remote> dropbox` for a new remote, `rclone config update <remote>
   config_refresh_token=true` for an existing one whose access ended (or with **Sign in to
   Dropbox again**, for another account). Both run with `config_auth_no_browser=true`, so rclone
   opens no browser, and `--no-output`, and rclone's standard output stays closed: it would end
   with the remote, token included. From rclone's notices SlopBar takes only its loopback link
   (`http://127.0.0.1:53682/auth?state=…`, exactly that form), which it opens in the browser (with a
   notification that opens it again: Windows can keep a page opened from the background behind
   other windows; Settings turns either off, `sharing_open_browser` and
   `notifications.sharing_sign_in`, both on by default), and
   the reason a failed sign-in gives: `denied` (not allowed on Dropbox's page), `stale_page`
   (Allow clicked on a page from an earlier try), `port_busy` or `port_reserved` (port 53682 held
   by another program, or reserved by Windows). The user has 10 minutes. Stopped before Allow,
   rclone writes nothing. A failed listing of the remotes stops the setup instead of creating one:
   `config create` would replace a remote of that name.
3. **Sharing on.** `presence_remote` is set to the remote, and the setup lists the other computers
   seen there within the last day. A computer that shares through a folder is refused
   (`uses_folder`) rather than moved off it.

rclone and the tunnel below run in the app's job object, so they end with SlopBar, and an
automatic update waits while a setup runs here or the tunnel for another computer is open. Every
step has a time limit, and a setup that panics ends as failed (`setup_failed`), so that wait always ends.

**Another computer, over SSH.** `setup_sharing` with `ssh_host` sets up that computer from this
one, and Dropbox's page opens on that computer's own screen, in its browser, as if the setup had
started there (the owner's ruling: the page opens on the computer being set up). It is refused
while nobody can see that desktop, like every sign-in SlopBar starts. Only with `open_here: true`,
for a computer whose screen is out of reach, does the page open on this computer instead: that
computer's SlopBar gets rclone and starts the sign-in without a browser, and this computer's
SlopBar reads only the link's `state` value (`slopbar-mcp --sharing-state`, never
a tool result), checks that it is letters, digits, `-` and `_`, opens an SSH tunnel
(`ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:53682:127.0.0.1:53682 -L
[::1]:53682:127.0.0.1:53682`; Dropbox sends the browser to `localhost`, which browsers try over
IPv6 first) and opens the link it builds from that value in this computer's browser. Dropbox's
answer reaches that computer's rclone through the tunnel, so the token never passes through this
computer. The tunnel closes once `get_sharing_setup` with that `ssh_host` sees the setup end, and
12 minutes after it opened at the latest. A computer reached over SSH can never set up a third one.

## Rejected: reusing browser sign-ins

Reading the user's browser cookies to reach their cloud storage, or borrowing another app's OAuth
client id, is ruled out:

- **It is credential extraction.** Decrypting another program's session cookies is what
  infostealer malware does. SlopBar would hold full-account sessions it has no business holding,
  far beyond one app folder.
- **Chrome app-bound encryption.** Since Chrome 127, Chromium browsers on Windows bind cookie
  encryption to the browser itself. Reading the cookies means defeating that protection.
- **Provider terms.** Dropbox, Google and Microsoft require API access through the app's own
  registered client. Session-cookie scraping and impersonating another client break their terms
  and get accounts flagged.

rclone avoids all three. The user signs in through rclone's own OAuth flow, rclone uses its own
registered clients, and SlopBar never touches the credentials.

## Rules every API backend follows

- **Tokens.** OAuth tokens go in DPAPI-protected storage through `private_store::save_secret`
  and `load_secret`, like the monitoring credentials, for example
  `<app dir>/presence/<provider>.bin`. They never go in `settings.json`, logs or diagnostics
  exports. `settings.json` records only which backend is chosen.
- **Sign-in flow.** Use the authorization-code flow with PKCE (S256) and a loopback redirect on
  `127.0.0.1`. The sign-in runs in the user's browser. SlopBar never sees a password.
- **Layout.** Use one file per machine, `presence/<machine id>.json`, overwritten in place. Read
  every file in that folder. `presence::peers` already handles conflict copies, clock skew and
  staleness.
- **Signing out.** Revoke the refresh token where the provider supports it, delete the DPAPI
  blob, and `clear()` this machine's record first.
- **Tests.** Use synthetic identities only, and never contact the real provider in tests.

## Wiring a backend in, once its app is registered

1. Put the client id in `CloudApi::client_id`, and the client "secret" for Google's Desktop
   client type. Neither is confidential for installed apps.
2. Implement `list` and `write` against the provider's API, as described below.
3. Add `presence_cloud: Option<CloudProvider>` to `Settings`. Have `presence::store` return
   `CloudApi` when it is set, next to the existing rclone and synced-folder choices.
4. In Settings > Data, offer "Connect Dropbox…", "Connect Google Drive…" and "Connect OneDrive…"
   next to the detected folders. Run the OAuth flow, store the tokens as above, then refresh.

## Dropbox (App folder, OAuth PKCE)

1. Sign in to <https://www.dropbox.com/developers/apps> with the owner's account and choose
   **Create app**.
2. Choose **Scoped access**, then **App folder** (not "Full Dropbox"). Name the app, for example
   `SlopBar`. The folder becomes `/Apps/SlopBar` in each user's Dropbox.
3. On the **Permissions** tab, tick `files.metadata.read`, `files.content.read` and
   `files.content.write`, then **Submit**.
4. On the **Settings** tab:
   - Set **Allow public clients (Implicit Grant & PKCE)** to *Allow*. PKCE then needs no app
     secret.
   - Under **Redirect URIs**, add the exact loopback URI SlopBar will listen on, for example
     `http://127.0.0.1:53682/oauth/dropbox`. Dropbox matches redirect URIs exactly, so fix the
     port.
   - Copy the **App key**. It is the client id.
5. Request `token_access_type=offline` so Dropbox returns a refresh token.
6. API: `files/upload` (mode `overwrite`) to `/presence/<machine id>.json`, then
   `files/list_folder` and `files/download` to read. Paths are relative to the app folder.
7. A new app is in *development* status and can be linked by a limited number of users (500 at
   the time of writing). Apply for **production** on the app's page before wide release.

## Google Drive (`drive.appdata`, Desktop OAuth client)

1. At <https://console.cloud.google.com>, create a project, for example `SlopBar`.
2. Go to **APIs & Services > Library**, find **Google Drive API** and **Enable** it.
3. Open **Google Auth Platform**, also shown as **OAuth consent screen**:
   - **Audience:** set User type to *External*.
   - **Branding:** set the app name, support email and developer contact.
   - **Data access:** add the scope `https://www.googleapis.com/auth/drive.appdata`. Google
     lists it as non-sensitive, so it needs no scope verification. Confirm the classification the
     console shows when adding it.
4. **Publishing status matters.** While the app is in **Testing**:
   - only listed test users (up to 100) can sign in;
   - **refresh tokens expire after 7 days**, so every machine would have to sign in again
     weekly.

   Choose **Publish app** to move it to *In production* before anyone relies on it.
5. Go to **Clients**, also shown as **Credentials**, choose **Create client**, and pick
   application type **Desktop app**. Copy the client id and client secret. For Desktop clients
   the secret is not confidential, and Google accepts any loopback redirect
   (`http://127.0.0.1:<port>`) without registering it.
6. API: create files with `parents: ["appDataFolder"]` (`files.create`), list them with
   `files.list?spaces=appDataFolder`, and update them with `files.update`.
7. The appDataFolder is hidden and private to this OAuth client's project. Every machine must
   use the same client id and the same Google account to see the same records.

## OneDrive (Microsoft Graph `Files.ReadWrite.AppFolder`, public client)

1. At <https://entra.microsoft.com>, go to **Identity > Applications > App registrations** and
   choose **New registration**.
2. Name it, for example `SlopBar`. Under **Supported account types**, choose *Accounts in any
   organizational directory and personal Microsoft accounts*.
3. Under **Redirect URI**, choose platform *Public client/native (mobile & desktop)* and enter
   `http://localhost`. Loopback ports are matched loosely for this platform.
4. Go to **Authentication > Advanced settings** and set **Allow public client flows** to *Yes*.
   The app uses no client secret.
5. Go to **API permissions > Add a permission > Microsoft Graph > Delegated permissions** and add
   `Files.ReadWrite.AppFolder` and `offline_access`. `User.Read` is there by default.
   - Personal accounts consent for themselves.
   - Work or school tenants may require admin consent, depending on tenant policy.
6. Copy the **Application (client) ID**. It is the client id. Use the authority
   `https://login.microsoftonline.com/common` with PKCE.
7. API: `PUT /me/drive/special/approot:/presence/<machine id>.json:/content` to write, and
   `GET /me/drive/special/approot:/presence:/children` to list. The folder appears as
   `Apps/SlopBar` in the user's OneDrive. Check `approot` support against work and school
   accounts before relying on it there.
