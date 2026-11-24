<div align="center">

<img src="design/icon.png" width="112" alt="SlopBar icon" />

# SlopBar

Your Claude Code and Codex plan limits, right in the Windows taskbar, next to the clock.

<!-- TODO: screenshot of the taskbar widget and its flyout -->

</div>

## Features

- **Account discovery**: reads the credentials Claude Code and Codex already saved.
- **Independent monitoring**: connect each account once to keep checking its usage while it is inactive, without opening a conversation or switching the CLI.
- **Windows + WSL**: finds accounts in your Windows profile and in every WSL distro.
- **Lives in your taskbar**: a small widget next to the clock shows each provider's usage as a ring around its logo; hover for the exact number.
- **One click for the details**: click the widget for a flyout with every account, its session and weekly limits, reset times, limit resets and credits. Right-click it to refresh, open Settings or quit.
- **Multiple accounts**: add as many Claude and Codex accounts as you like and switch with one click, or let it switch for you when one hits its limit.
- **Usage by project and model**: Settings > Usage shows each account's tokens by repository and model over the last 7 or 28 days, its busiest hours of the week, and each provider's model mix, read from Claude Code's and the Codex CLI's own files on this PC (token counts, models, times and folders only, never what was written). Claude use is credited to the account SlopBar saw signed in at the time. Export the weekly history, this breakdown and the subscription report as CSV files.
- **Feels native**: follows the taskbar's light or dark theme, and steps aside while a fullscreen app is running.
- **Speaks your language**: follows the system language (10 languages, English fallback).

> [!NOTE]
> SlopBar was formerly called Usage Bar. Existing Usage Bar installs update into SlopBar automatically.

## Multiple accounts

Add accounts in **Settings → Accounts**: SlopBar opens the CLI's own sign-in (`claude auth login` / `codex login`) in an isolated folder, so your current login is untouched. It uses the CLI on PATH as Windows has it now (one installed while SlopBar runs counts, with no restart), in its installer's usual folder (even one the installer left off PATH; a terminal, and `slopbar-run`, need that folder on your user PATH), in a WSL distro, or, for Claude Code, the copy the Claude desktop app keeps for itself. Without one, install it from PowerShell and add the account again: Claude Code with `irm https://claude.ai/install.ps1 | iex` (or `winget install Anthropic.ClaudeCode`), the Codex CLI with `powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"` (or `npm install -g @openai/codex`). Switching rewrites the CLI's login files in place; the official CLI keeps doing all the work. Switch from the flyout with **Use**, or turn on **Switch automatically** to move to the account with the most headroom when the active one reaches your threshold.

> [!NOTE]
> A running Codex session keeps its account until you restart it.

### Turning sharing on

In-use sharing needs a place all your computers reach: a folder a sync app mirrors (OneDrive, Dropbox, Google Drive, iCloud Drive, Box), or a cloud remote through rclone. On a computer with neither, **Set up** in Settings → Data → Other computers (also in the getting-started guide) does it in one go: it downloads rclone if no recent copy is installed (one fixed release, checked against its published SHA-256, into SlopBar's own folder; on a network that blocks it, the setup says how, such as a certificate Windows doesn't trust or a web filter's page), opens Dropbox's page in your browser for you to sign in and click **Allow** (a notification offers it too; Settings turns either off), then turns sharing on and lists the other computers it finds there. Use the same Dropbox account on every computer. rclone keeps the Dropbox token in its own config, so SlopBar never sees it; rclone's access covers your whole Dropbox, while SlopBar only uses its `SlopBar` folder there. Agents do the same with the `setup_sharing` tool, also for another of your computers over SSH: the page opens on that computer's screen, as if you had clicked Set up there.

### Which computer uses which account

With in-use sharing on (Settings → Data → Other computers), wherever SlopBar shows an account it names the computers the account is in use on, this PC first and marked: "In use on DESK-1 (this PC, Desktop app), LAPTOP-2 (WSL: Ubuntu)". That is the account's tooltip in the flyout and in the taskbar widget, a short line under its name in the compact flyout, the cards in the full flyout and in Settings → Accounts and Subscriptions, the notification about an account in use on two computers, and the quick switcher, where typing a computer's name finds the account it uses. The names are the ones sharing already carries, so nothing more is shared; with sharing off, no computer's name is shown.

### One account on two computers

With in-use sharing on (Settings → Data → Other computers), SlopBar flags an account that is in use on more than one of your computers at the same time: an amber "In use on 2 machines" line in the flyout and in Settings → Accounts, an amber badge on that provider's ring in the taskbar widget, a Health warning, and one notification per overlap (Settings → Notifications). Their use adds up, so its limits drain that many times as fast. An account that stayed under its weekly limits, model caps included, in each of its busiest recent weeks (the latest 8 completed weeks this computer watched, at least 4 of them watched long enough to judge) is shared within its limits: a planned share. It shows a plain "Shared by 2 computers" instead, with nothing amber, Health lists it as ok, no notification comes, and `recommend_account` does not move a computer using it off it (an account in use on other computers only is still left to them). A computer counts only while it keeps writing its status, so one that was shut down drops out within about 15 minutes.

### A new computer

Turn on **Share account list with my other computers** (Settings → Data → Other computers) on each computer, with the same roster passphrase on each. Every SlopBar then adds its accounts' provider, id, email and plan (never sign-ins) to the sharing folder or rclone remote, encrypted. A computer that reads them shows "N accounts from your other machines are waiting to be signed in" in the flyout and Health, and **Sign in all accounts** (Settings → Accounts) queues every one, one email at a time. Moving sharing elsewhere, or turning the list off, removes this computer's list from where it was at the next refresh, or as SlopBar quits. When a folder that does not answer, or a crash, keeps that from happening, it is done later, after a restart too: the other computers would otherwise keep reading a list left there for 60 days.

Each list also carries the latest usage reading of each account that computer reads itself (**Share usage readings**, on by default whenever the list is on). A computer whose own monitoring sign-in for an account has ended, or was never connected, then shows the newest of those instead of its own old reading, marked with whose it is and how old: "Reading from LAPTOP-2, 12m ago". A reading up to 7 days old is shown, with its age, and a fresh reading of its own always wins. So one computer's monitoring sign-ins can cover the others: while another computer still reads an account (its own sign-in for it still works, and it read it under 12 hours ago), Health calls this computer's ended sign-in for it a warning rather than a problem, and the flyout doesn't count it among the sign-ins to reconnect. A reading from a computer whose own sign-in has since ended is still shown, with its age, but covers nothing. These readings are only shown: automatic switching, alerts, history and forecasts here still go by this computer's own readings, which a monitoring sign-in here brings back. A changed reading on its own is written at most every 15 minutes, and the lists are read every 10 minutes.

### Bring up another computer

An agent with SSH to the new computer (OpenSSH, over your network or Tailscale) can do all of that from a computer that has your accounts, with no passphrase for anyone to type: `get_machine_readiness` checks the new computer, `pair_machine` gives both the same roster key over the SSH connection (generated when there is none, never shown), and `run_on_machine` lists the accounts waiting there and starts **Sign in all accounts** on its desktop once you are at its screen. Once paired, its monthly sign-ins can also be made from any of your computers that reaches it over SSH (see **Every computer from one desk** below). The [field guide](docs/remote-setup.md) covers setting up SSH and what to ask the agent.

### Monitoring accounts you aren't using

While an account is the one signed in to the CLI, SlopBar reads its usage with the CLI's own access token. That stops when you switch away: the CLI's refresh token rotates every time it is used, so SlopBar can't keep renewing a copy of it in the background without signing the CLI out. To keep reading an account you aren't using, SlopBar needs its own sign-in for it, a separate session it alone renews.

SlopBar starts that sign-in for you. Once an account without monitoring has been the one in use for 3 minutes, it opens the sign-in and shows a notification, **Finish connecting monitoring for** that account. The page opens where Settings → Accounts says sign-ins open: by default a fresh browser profile, where you sign in as that account; in your own browser, one already signed in to it needs only an approval. It runs one sign-in at a time, waits while a fullscreen app is in front, never retries a cancelled or failed sign-in for that account within 24 hours, and leaves alone an account whose monitoring you disconnected. Turn it off with **Settings → Data → Connect monitoring automatically**.

You can also choose **Connect monitoring** next to each account in Settings or the flyout. Complete the official CLI's sign-in for that same account and organization. This creates a separate monitoring session in a temporary configuration folder; it is never imported into the switching vault or written to the active CLI's files. The temporary folder is removed when sign-in completes, fails, or times out.

SlopBar renews this separate session automatically when its access token is about to expire, and polls the provider's usage endpoint. No prompt or model request is sent. Existing accounts need this separate sign-in because their copied CLI refresh tokens cannot safely be shared with an independent background refresher.

**Claude sign-ins end after about 30 days.** Anthropic ends every Claude sign-in on a date it sets when the sign-in is made, about 30 days later, and renewing never moves that date. Claude Code's own sign-in works the same way. No setting makes one permanent, so each Claude account needs a new monitoring sign-in about once a month; Codex sign-ins have no such date. Health says when a monitoring sign-in ends within 3 days and when it has ended, the flyout counts them, and a **Reconnect monitoring** button appears next to each one. **Connect monitoring for all** (Settings → Accounts, or **Connect all** in Health) signs in again every account that needs it, one after another, including any that end within the next 7 days, so they all end together next time. Signing in again needs no account use and switches nothing. SlopBar doesn't start these sign-ins on its own: by default each opens a fresh browser profile that needs a full sign-in, so it waits until you choose to. With the account list shared between your computers (see [A new computer](#a-new-computer)), your computers need not all sign in again: one computer's readings show on the others, and Health there says the ended sign-in is covered.

**Saved CLI sign-ins end too.** The login SlopBar keeps for each account, which it writes back when you switch the CLI to that account, is a Claude Code sign-in with the same 30-day end. Switching to an account whose saved sign-in ended stops the CLI with "this login reached its time limit" until it signs in again. So for every account not in use, Health warns when its saved CLI sign-in ended or ends within 3 days, its row in Settings, the flyout and the quick switcher says so next to **Use** (Use in Settings asks first, and the switcher lists it last and asks for a second Enter), `get_usage` carries the date (`cli_ends_at`, `cli_renewal`), and automatic switching, `recommend_account` and `lease_account` pass it over. **Sign in all accounts** (Settings → Accounts, or **Sign in all** in Health) makes each of them a new CLI sign-in for the same account, including any that end within the next 7 days, in the same sitting and the same fresh browser window as that email's monitoring sign-ins; with **Also connect monitoring** off it makes only these. It replaces the saved login and nothing else: no account is switched, nothing is signed out, and the old sign-in simply runs out. The account in use is never touched: Claude Code manages that sign-in. Health still says when that one ended or ends within 3 days, since an agent running the CLI never sees Claude Code's own notice; run `claude /login` for it.

**Every computer from one desk.** With your accounts signed in on several computers, each needs those sign-ins again every month. **Sign in all your computers from here** (Settings → Accounts, shown once SlopBar knows another of your computers; Health's renewal summaries offer it too) does them all in one sitting at whichever computer you are at: this computer's sign-ins, and those each of your other computers needs, reached over SSH (computers paired or checked from here, and those the shared account list names when `~/.ssh/config` has their name). Each other computer runs its own sign-in exactly as **Sign in all accounts** would run it there, but with no window, and hands its page over; SlopBar opens an SSH tunnel for the one local port that page sends the browser back to, and opens the page here in that email's fresh browser profile. The provider's answer goes from this browser through your own SSH straight to that computer's Claude Code or Codex, which finishes the sign-in itself: SlopBar on this computer never reads the sign-in code or a token: the code passes only through this browser and your SSH connection, no token leaves the other computer, and each sign-in is saved only there, by its SlopBar, as a sign-in at its own screen would be. An email's pages for every computer open one after another in the same fresh profile, so Google is asked once per email. Nothing opens on the other computers' screens, so they can stay locked. Another computer's monitoring sign-ins are included only where it reads usage for itself (automatic switching or a rotation threshold) or this computer's shared readings wouldn't reach it (this computer doesn't read that account, doesn't share usage readings, or that computer doesn't share the account list): shared readings cover the rest. Before anything starts, SlopBar lists each computer, its SSH host, its sign-ins and their emails, and asks you; **Retry** and an agent's request show the same list. Afterwards it names any computer left out and why (not reachable over SSH, not running, needing a newer version, busy), and **Retry** asks every computer again. A computer's account in use keeps its CLI sign-in as it is, since Claude Code manages that one, and is named with `claude /login` to run there; its monitoring sign-in is renewed as usual. Agents use `sign_in_all` with `everywhere: true`; with `confirm: true` it only shows you a notification here, whose **Review and start** opens that list, and nothing starts until you confirm it. Each of your other computers needs PowerShell 7 to start over SSH.

**A page that can't finish.** If a sign-in's page never comes back (the browser says "This site can't be reached", say), **Retry this sign-in** next to the progress line in Settings → Accounts, or **Retry** on the notification that it is still waiting, starts just that sign-in again: a new page opens in the same window (in the default fresh browser profile Google stays signed in there; a private window asks again), and the run goes on. One cause is handled for you: Claude Code's sign-in listens on whatever free local port Windows hands it, and on a computer whose dynamic port range starts low (`netsh int ipv4 show dynamicport tcp` shows 1024 rather than Windows' default 49152) that can be a port browsers refuse to open, such as 6667 (Chrome and Edge say ERR_UNSAFE_PORT). Wherever SlopBar opens the sign-in page (in a fresh profile, the default, or a private window, and always for another computer's sign-ins), it never opens such a page: it starts that sign-in again for another port, eight tries in all. With the sign-in window set to the default browser, or a CLI in WSL, the CLI opens its own page, so only Retry helps there. Setting the range back to Windows' default (`netsh int ipv4 set dynamicport tcp start=49152 num=16384`, as administrator, on each computer) avoids those ports altogether; it is a system setting, so SlopBar leaves it to you. On another computer's **Signing in on …** card in Settings → Accounts, once its sign-in has waited 5 minutes, **Retry** starts just that sign-in again there over SSH, in the same window, as an agent's `restart_sign_in` does; where that computer runs an older version, or its run has ended, it runs **Sign in all accounts** there again.

SlopBar's own connections trust what Windows trusts, through Windows' own certificate checks, and each goes through the proxy Windows would use for it, proxy scripts (PAC) and automatic detection included, as your browser does; its updater and rclone get the same proxy, and the proxy environment variables (HTTPS_PROXY, NO_PROXY) still come first. Where Windows names a proxy SlopBar can't use (a SOCKS proxy) or a proxy script that can't be fetched or run, or HTTPS_PROXY or ALL_PROXY names one it can't use, SlopBar doesn't connect around it and says so, for usage, monitoring and sharing alike (Settings > Health too); a proxy that asks for a Windows sign-in (NTLM, Kerberos) isn't supported, and SlopBar's own connections say that too. rclone takes one proxy for a whole cloud, so where a proxy script decides, SlopBar runs it only for Dropbox, Google Drive, OneDrive and Box, and only when the script gives that cloud's sites one proxy; otherwise it says why, and sharing through a folder you sync avoids this. SlopBar reads the proxy variables it was started with: after changing them, quit SlopBar and start it again (its own Restart keeps the old ones). Phone alerts to a server on this machine or the local network go to it directly. On a network that inspects HTTPS with its certificate installed in Windows they work, and that network can read them, sign-in tokens included, as it can read the browser's. A certificate Windows distrusts is refused.

**Disconnect monitoring** works even for the active account. It removes the local monitoring credentials and attempts to revoke that separate session at the provider. If server revocation cannot be confirmed, the UI asks you to revoke it in your provider's account settings. Reconnecting saves the new login before attempting to revoke the previous monitoring session.

If a completed monitoring login is rejected for the wrong account or cannot be saved, SlopBar attempts to revoke that uninstalled login. It first protects every known CLI/monitor token; shared tokens or unreadable ownership stores disable this cleanup. Failed cleanup can leave a provider session to revoke manually in account settings.

Last successful readings survive app restarts. If the provider cannot be reached, the UI shows **Last confirmed … ago** and retains the observed values. Where another of your computers has read the account since (with the account list shared), its reading shows instead, as **Reading from … ago**. A passed reset time does not imply a measured 0%. Automatic switching considers only fresh, successfully measured accounts. A revoked session, an unusable refresh response, or a lost credential file may still require **Reconnect monitoring**; recent account use is never the recovery instruction.

A refresh timeout before success headers does not immediately disconnect an account. SlopBar waits five minutes and allows one retry of its own monitoring token. Both the backoff and retry limit survive restarts. An explicit token rejection requires reconnecting; after a second uncertain response, automatic replay stops and the UI offers reconnecting. A successful HTTP response with an unreadable body requires reconnecting immediately, because replay could reuse an already consumed token. The providers can reject a retry if the timed-out request already rotated its token.

If the active account's monitor fails, SlopBar can still read fresh usage with the CLI's current access token. It never refreshes that CLI token, and keeps the monitoring error and Reconnect control visible alongside the fresh reading. This fallback is restricted to active accounts and respects usage rate limits. When a sign-in's saved end date passes, SlopBar still tries one renewal and lets the provider decide; Claude refuses it, and the account then shows **monitoring sign-in ended**.

When only a passed end date triggers renewal, a temporary failure, rate limit, or uncertain send does not suppress a still-valid monitoring access token. Usage polling continues with that token while refresh backoff and the replay budget remain enforced. Expired or usage-rejected access tokens are never returned this way; explicit refresh rejection and unreadable successful rotations still require reconnecting.

### Why the original version lost inactive accounts

At upstream commit `60a7c8e0835b4cd320dba3e0b1352d7a8161c9e1`, `providers::fetch_one` stopped before requesting usage when a copied Claude access token had expired. SlopBar deliberately never renewed tokens, and `accounts::capture` only obtained replacements when an official CLI had already refreshed its login. Inactive accounts therefore lost live telemetry. The UI translated an inactive expired token into “Not used recently,” hiding the authentication cause. The last-success cache lived only in memory, and its reset prediction could display 0% without a provider response.

Refreshing those existing copied refresh tokens would race the CLI's token rotation. This branch instead owns a separate refresh-token chain per monitored account, serializes refreshes, saves rotated tokens atomically, backs off after failures, and uses Windows DPAPI for monitoring credentials. The account-switching vault is encrypted with DPAPI too: plaintext slots from earlier releases are encrypted on the first start, and a slot is never deleted before its encrypted copy reads back correctly; see [SECURITY.md](SECURITY.md).

## Diagnostics for real-machine testing

This build records a local rolling diagnostic history automatically. After testing, use **Settings → General → Diagnostics → Export diagnostics**. It creates a `SlopBar-Diagnostics-*.json` report and opens its folder on Windows. Upload that JSON report for analysis; do not upload the account vault, monitoring files, or the encrypted journal.

The report includes anonymous account numbers stable across restarts, usage percentages and last-confirmed times, monitoring/CLI/fallback usage outcomes, refresh HTTP statuses and timeout categories, renewal reasons, backoff/replay decisions, isolated login stages, revocation outcomes, poll durations/gaps, frontend error categories, and the executable's embedded build metadata. It excludes tokens, emails, aliases, account/organization IDs, home paths, distro names, request URLs/headers/bodies, provider response bodies, raw exception messages, and stacks. Collection is local; there is no automatic upload.

The journal retains at most **7 days or 10,000 events**, whichever limit is reached first. Unchanged token, usage and account observations are suppressed, with hourly heartbeat readings; state changes and unusual polling gaps are kept. Stable 11-account tests, including repeated monitoring errors with a working active-CLI fallback, cover a full week even at one-minute polling. Each usage origin has an independent state/heartbeat, so the monitor and fallback do not overwrite each other’s observations. Frequent real changes or repeated launches can still reach the event cap sooner; reports show the actual history start, discarded events and suppressed observations. Its account-number mapping is retained locally for continuity and never exported. Windows protects the journal/mapping with current-user DPAPI; exports are readable JSON containing usage/timing data. A background writer batches routine changes at most once every five minutes when needed. Authentication/lifecycle transitions request an immediate background write; no diagnostic serialization, DPAPI or disk I/O runs on the authentication caller. Export and orderly shutdown wait for queued writes. Killing the app can lose queued transitions and up to five minutes of routine changes, but the next startup records the missing clean shutdown. Storage failures are best effort and do not change token handling. A full crash dump and provider-side/CLI internal logs are not collected.

For automated build verification, `slopbar.exe --build-info-to <path>` writes the executable's own commit, tree, dirty-checkout flag, and Tauri/updater configuration without launching the GUI or reading credentials. CI packaging checks this output against the checkout before producing the ZIP.

### Agent access (MCP)

`slopbar-mcp.exe`, built next to `slopbar.exe`, is a stdio [MCP](https://modelcontextprotocol.io) server that gives coding agents what the Settings window offers, without driving the UI. Register it with your agent. For Claude Code, from PowerShell:

```powershell
claude mcp add --scope user slopbar -- "$env:LOCALAPPDATA\SlopBar\slopbar-mcp.exe"
```

From Command Prompt, write the path as `"%LOCALAPPDATA%\SlopBar\slopbar-mcp.exe"` instead. Either way the shell expands the path before Claude Code stores it. Claude Code runs the stored command exactly as written and does not expand `%LOCALAPPDATA%` itself, so if you type the `%…%` form into PowerShell or Git Bash, the server is registered but never starts. Check with `claude mcp get slopbar`: it shows the stored path and should report the server as connected.

For Codex, in `~/.codex/config.toml`:

```toml
[mcp_servers.slopbar]
command = 'C:\Users\<you>\AppData\Local\SlopBar\slopbar-mcp.exe'
```

| Tool | Does |
| --- | --- |
| `get_usage`, `refresh_usage` | Every account's usage windows, resets, monitoring state and errors (as last refreshed, or polled now). While an earlier refresh keeps it waiting for more than 30 seconds, `refresh_usage` (and any tool that refreshes after acting) returns the last numbers with a `waiting_for_refresh` note; a refresh follows as soon as that one ends. |
| `get_settings`, `update_settings` | Read settings, or change them with a JSON merge patch. Only values the Settings window offers are accepted; unknown fields are refused. |
| `switch_account`, `remove_account` | Use or remove a saved account, as in Settings. A switch's `switched_to_requested` is `null` when SlopBar was too busy to refresh and tell. |
| `add_account`, `connect_monitoring`, `disconnect_monitoring` | Start the CLI's own sign-in for the user to complete, or revoke a monitoring sign-in. |
| `revoke_account_sessions` | Revoke every sign-in SlopBar holds for an account (its monitoring session, at the provider and here), optionally removing the saved CLI login with `remove_slot`. Only reports unless `confirm: true`; the CLI's own sign-in is left to the CLI. |
| `check_for_updates`, `install_update`, `get_app_info`, `open_settings` | Updates (builds with automatic updates only), version and build, the last update check (time, result, and error or the reason it was deferred), and showing the Settings window. |
| `quit_app`, `restart_app` | Quit as the tray's Quit does, or quit and start the installed executable again. |
| `get_usage_breakdown` | Tokens on this PC by account and repository, model or day, with each provider's model mix and an hour-of-week heatmap, and how each account's tokens were attributed. |
| `export_csv` | Write the weekly history, the usage breakdown and the subscription report as CSV files (default `Documents\SlopBar Exports`) and return their paths. |
| `export_diagnostics`, `list_diagnostic_exports` | Write a diagnostics export (a repeat within 30 seconds returns the same file) or list earlier ones; listing works while SlopBar is closed. |
| `export_calendar` | Write the plan-renewal calendar (below) now and return its path. |
| `get_machine_readiness` | Whether this computer, or another over SSH (`ssh_host`), can take sign-ins: SlopBar running, someone at the desktop (not locked or disconnected), a browser, the roster and the accounts waiting. Works while SlopBar is closed. |
| `pair_machine`, `run_on_machine` | Pair another computer's account roster over SSH with a key nobody types or sees, and run a few of its tools there (`get_roster`, `sign_in_all`, `restart_app`…). Sign-ins open on that computer's desktop and are refused while nobody can see it. See the [field guide](docs/remote-setup.md). |
| `lease_account`, `release_lease`, `list_leases` | Reserve an account for a job (`purpose`, `minutes`, optionally `expected_percent` or a given `id`): until the lease ends, `recommend_account` lists it as excluded ("leased: …") for everyone else, automatic switching never moves to it and the flyout shows the purpose on its row. A lease switches nothing; it returns the `config_dir` to run the job with. Leases expire (default 2 hours, at most a day) and end when SlopBar quits. |

The server never starts SlopBar, reads credentials or contacts a provider itself: each tool asks the running app, which performs the same action as the matching Settings control. They talk over a named pipe that only the signed-in user and SYSTEM can open and that rejects remote clients. The server checks that the process answering runs as the same Windows user, connects so that process cannot act as the agent's, bounds every reply in size and time, and only passes on an export path inside the export folder. Sign-ins always happen in the CLI's own login, completed by the user. Usage results include account emails, so treat them like the Settings window. `slopbar-mcp.exe` is a separate executable so that app updates, which stop `slopbar.exe`, do not cut agents off mid-session.

### Claude Code status line

`slopbar-mcp.exe --statusline` prints one line about the Claude account in use, for Claude Code's [status line](https://docs.claude.com/en/docs/claude-code/statusline), for example `work · 12% 5h · 42% wk · runs out Fri 3PM`. It asks the running app, reuses an answer for 15 seconds, gives up after 120 ms and prints nothing when SlopBar is not running. To use it, add this to `~/.claude/settings.json` (forward slashes keep the JSON simple):

```json
{
  "statusLine": {
    "type": "command",
    "command": "C:/Users/<you>/AppData/Local/SlopBar/slopbar-mcp.exe --statusline"
  }
}
```

### Exact Claude attribution

Claude Code's transcripts do not name the account they ran on, so the usage breakdown otherwise attributes Claude use to the account SlopBar saw signed in at the time (`inferred_from_active_account`), and use from times SlopBar was not watching stays unattributed. `slopbar-mcp.exe --session-hook` is a [SessionStart hook](https://docs.claude.com/en/docs/claude-code/hooks): when a session starts it tells the running app the session id, working folder and `CLAUDE_CONFIG_DIR` (unset means the default login), and the app records which account that config dir is signed in to. The breakdown then attributes that session's transcript, subagents included, to that account (`exact`) ahead of any inference. It prints nothing, waits at most 1.5 seconds for SlopBar and always exits 0, so a closed SlopBar never holds up Claude Code. Only the session id, folder and account id are kept (in `active-accounts.json`, 90 days); no emails. Add this to `~/.claude/settings.json` yourself (SlopBar never edits it):

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "C:/Users/<you>/AppData/Local/SlopBar/slopbar-mcp.exe --session-hook" }
        ]
      }
    ]
  }
}
```

Merge it into an existing `hooks` object if you have one. `SLOPBAR_HOOK_DEBUG=1` makes the hook say on stderr what happened.

### Graceful rotation (an account kept for the web)

Settings > Accounts > a Claude account > **Rotate off at** gives that account its own usage percentage (`rotate_at`, 50–95%). It suits an account you also use on claude.ai and never want coding to drain: at that percentage SlopBar rotates off it even with automatic switching off, and automatic switching never moves back to it until it is 10 points below.

The rotation is graceful. Running Claude Code sessions on the account finish the task they are on, then save their state before the switch:

- a prompt submitted on the account gets context saying it is rotating: finish this task and don't start new work;
- the first time each of its sessions ends a turn, SlopBar blocks that stop once with the wrap-up instruction: update durable memory and each project's canonical state on disk (handoff, checkpoint or status files, following that project's own conventions), then end the turn;
- once no session is still busy or wrapping up, SlopBar switches to the account it would pick for automatic switching.

It never waits past `rotate_grace_minutes` (20 by default, 5–120) or past 5 points above the percentage (at most 98%), whichever comes first. Sessions idle when the rotation starts are not waited on. Only sessions on the default login take part, since a switch never moves a project profile. A Windows notification says when a rotation starts; the usual switch notice says when it is done.

The turn hook needs these entries (and the SessionStart hook above, so sessions are tied to their account from the start):

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "C:/Users/<you>/AppData/Local/SlopBar/slopbar-mcp.exe --turn-hook" } ] }
    ],
    "Stop": [
      { "hooks": [ { "type": "command", "command": "C:/Users/<you>/AppData/Local/SlopBar/slopbar-mcp.exe --turn-hook" } ] }
    ]
  }
}
```

It prints nothing unless the session's account is rotating, sends only the session id, the event and `CLAUDE_CONFIG_DIR` through the door (never the prompt), waits at most 1.5 seconds and always exits 0.

### Smart launcher (`slopbar-run`)

SlopBar keeps a launcher at `%APPDATA%\dev.thelio.slopbar\bin\slopbar-run.cmd` (Settings > Accounts > Projects shows the exact path). Put that folder on your `PATH`, then:

```
slopbar-run [claude|codex] [--provider <claude|codex>] [--lease <purpose>] [--percent <n>] [-- <command> [args...]]
```

- Inside a folder pinned to an account, the command runs on that account's project profile (as before).
- Anywhere else it asks SlopBar for the account `recommend_account` would pick among those nobody else leased (`--percent`: the share of a week the job needs). When that is not the CLI's current sign-in, it leases the account for as long as the command runs, so the account's project profile stays signed in and other agents keep off it, and sets `CLAUDE_CONFIG_DIR` or `CODEX_HOME` for that one command. `--lease <purpose>` names the job and leases the account even when it is the current sign-in. The lease is released when the command exits (and ends by itself if the launcher is killed).
- The provider defaults to the command's (`codex` runs Codex, anything else Claude Code); the command defaults to the provider's CLI. Examples: `slopbar-run claude`, `slopbar-run --lease "nightly refactor" -- claude -p "…"`, `slopbar-run -- codex exec "…"`.
- Without SlopBar running, the command runs on the CLI's current sign-in. `SLOPBAR_RUN_DEBUG=1` says why on stderr.

### Renewal calendar and Monday digest

**Settings → Subscriptions → Renewal calendar** keeps an iCalendar file (`Documents\SlopBar\renewals.ics` unless you pick another path) with an all-day event for each paid plan's next renewal, and a reminder three days ahead for plans that look unused or underused. Subscribe to the file from Outlook (**Add calendar → From file**) or import it into Google Calendar; with **Keep a renewal calendar** on, SlopBar rewrites it after each refresh, and each renewal keeps the same event ID across rewrites, so subscribed calendars update rather than duplicate it. **Export now** writes it once.

The **Monday digest** (Settings → Notifications, on by default) is one notification on Monday mornings with what your paid plans cost per month, what you could save, the plans renewing that week, accounts that ran out in the past week and how many plans your usage needs. Clicking it opens Settings → Subscriptions.

To install a local release build without quitting SlopBar by hand, run `scripts/install-local-build.ps1` after `cargo build --locked --release --features tauri/custom-protocol --manifest-path src-tauri/Cargo.toml`. It checks the build matches the clean checkout, moves the running executables into a rollback folder under `%LOCALAPPDATA%\SlopBar-Rollback`, copies the new ones in and restarts SlopBar through `restart_app`.

## Build

On Windows, with Rust and the usual Tauri prerequisites installed:

```powershell
cargo test --locked --no-default-features --lib --manifest-path src-tauri/Cargo.toml
cargo build --locked --release --features tauri/custom-protocol --manifest-path src-tauri/Cargo.toml
```

The standalone executable is `src-tauri/target/release/slopbar.exe`. Exit any running SlopBar before starting it; the single-instance guard otherwise hands control to the already running version. Existing settings and accounts are read from the same data directory.

The `tauri/custom-protocol` feature selects Tauri's release configuration with embedded assets. CI checks and test builds use this feature too; a plain Cargo release build without it uses Tauri's development configuration.

Local builds disable upstream updates at compile time, including if an existing settings file enables automatic updates. This prevents an unpatched upstream release from replacing this fix. Release maintainers can explicitly enable the `upstream-updates` Cargo feature after merging the fix. The Settings page shows when updates are unavailable.

An optional frontend smoke check is available as `node scripts/test-telemetry-ui.cjs` with Playwright and Chromium installed. It uses synthetic accounts and mocked Tauri commands to check monitoring controls, stale usage, error feedback, and the minimum supported window width. Set `CHROMIUM_PATH` to use an existing Chromium binary. These checks do not authenticate with a provider or exercise a real Windows credential store; the Rust tests run DPAPI checks when executed on Windows.

To check that a click on the taskbar widget really draws the flyout in the running app, run `powershell -ExecutionPolicy Bypass -File scripts/test-flyout-click.ps1` on an unlocked desktop. It posts a click to the native widget, fronts and captures the flyout from the screen with BitBlt (not PrintWindow, which repaints and would hide a flyout that is shown but never composited), closes it with a second click and captures what was behind it. It prints the time until the flyout is visible, its distinct colour count and how much it matches what is behind it, then PASS or FAIL: it fails when the flyout is not visible within 2 seconds, matches the background by more than 50%, or has fewer than 20 colours. Both captures are saved as PNGs in `%TEMP%` and can show account emails, so do not attach them to public issues. It refuses to run without a foreground window (a locked screen or disconnected remote session), where no capture means anything.

The Rust suite also runs the real HTTP code against a local server with synthetic tokens: JSON request encoding, saved Claude scopes, nested OAuth errors, rate limits, delayed responses, and revocation. It covers failed credential-file deletion after handoff, non-revocation of imported sessions, and successful-response bodies that must never be replayed. Windows tests check suspended process startup and descendant termination. `Telemetry tests` runs core tests on Linux and Windows, then checks the official updater feature and builds an unsigned native Windows desktop executable. Its test-build artifact includes the exact source and SHA256 manifest; it does not publish a release or use signing secrets. Fork PR workflows can require maintainer approval before they execute.

Cross-compiled for Windows from WSL/Linux:

```bash
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin
sudo apt install -y nsis lld llvm clang
pnpm install
pnpm tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --no-bundle
```

The app is at `src-tauri/target/x86_64-pc-windows-msvc/release/slopbar.exe`.

> [!NOTE]
> Monitoring only renews its own separately connected sessions. Imported CLI sessions are never refreshed by SlopBar.

## Releases

The upstream release workflow publishes from `main` (`v0.2.<run>`), runs the telemetry tests on Windows, and enables signed upstream updates for those releases. Local builds use manual updates.

Builds with updates check about 20 seconds after starting and then every 6 hours, and install what they find while **Update automatically** is on. What is bounded, and by how much:

- the check's request: 30 seconds; any request, the download's included, without data: 60 seconds; the download: 30 minutes in all;
- each step of an install, whose error then names it: the check 40 seconds, the download 31 minutes, handing over to the installer 1 minute;
- a whole check and install: 35 minutes 40 seconds, in a task of its own, so a panic or this limit ends that attempt and nothing else;
- SlopBar's exit cleanup: 15 seconds for each part that can wait, and 3 seconds for the diagnostics journal.

The installer itself is a separate program that SlopBar does not bound once it has started. One install runs at a time, from its download to the end of its handover; a check alone takes no turn. Another install asked for meanwhile is deferred, not failed, or fails once the running one is past the 35-minute limit. A handover that outlives its limit cannot be stopped: its thread carries on and keeps the install's turn until it ends, so one that never returns keeps it until SlopBar restarts. A failed attempt is tried again after 2 minutes, once, and then at the usual interval; a deferred one waits the interval.

`get_app_info` reports the last check as `update_check`: its time, its result (`up_to_date`, `found`, `installing`, `deferred` or `error`), version, error or the reason it was deferred, and when the next check runs. The record is kept in memory only. After an install starts, its last word is `installing`, because the process that kept it exits, and the new version starts with no record until its first check.

## License

[GPL-3.0](LICENSE). Use it, change it, share it; derivative works must stay open source under the same license.
