# SlopBar Ultra roadmap

Owner-approved on 2026-10-21: build every item below. Work drains in waves of at most three
parallel sessions, each in its own worktree branched from `claude/subscription-value`, so sessions
rarely edit the same Settings files at once. A wave's branches merge back into
`claude/subscription-value` (merge commits, no rebase) before the next wave starts. Every item
lands with tests on synthetic identities, a door (MCP) for anything the window can do, and a check
through `inspect_window`, never through the user's desktop.

This file lives only on the fork's `ultra` branch; status is updated here.

Status: `running` (a session has it), `queued`, `done` (merged; the commit is noted).

**Upstream PRs.** Every item also goes to the official product as its own pull request on
thelioo/slopbar. Its branch is cut at the item's merge commit, it targets `main`, and its
description names the PRs it stacks on. The work before this roadmap is #6, then #8, #9, #10 and
#11, merged in that order. This roadmap file itself stays on the fork and is never part of an
upstream PR.

## Wave 1: done (merged at b25d07f on claude/subscription-value)

| # | Item | Status |
|---|---|---|
| 1 | **Renewal warnings.** A notification 3 days before an unused or mostly idle plan renews, naming the account, its verdict and cost, with Manage plan one click away. A softer one for any paid plan renewing within 24 hours. | done (3aea1a9) |
| 2 | **Runs-out-at forecast.** "Hits its weekly limit Thursday around 3 PM" in the flyout and on Subscriptions, plus an optional heads-up for the account in use. | done (3aea1a9) |
| 3 | **Unexpected-use alert.** With sharing on everywhere, usage rising while no machine reports the account in use means the web or mobile app, or a leaked sign-in. | done (b25d07f) |
| 17 | **`recommend_account` door.** An agent asks which account has headroom for its job; SlopBar ranks them and can switch. | done (5ebc677) |
| 18 | **`get_headroom` door.** A long-running agent slows down or pauses before a limit instead of failing mid-task. | done (5ebc677) |

## Wave 2: done (merged at ed8d1f6 on claude/subscription-value)

| # | Item | Status |
|---|---|---|
| 4 | **Pro 200 allowance change on 2026-10-29.** Flag which Pro 200 accounts will start running out once the grandfathered allowance ends. Time-sensitive: first in this wave. | done (d271ebd) |
| 5 | **Smarter auto-switch.** Prefer the account whose weekly reset or renewal comes soonest and still has headroom; skip accounts in use on another machine and plans being cancelled. | done (d271ebd) |
| 10 | **Pool sizing.** Weekly peaks per provider against the capacity owned: "your Codex usage fits in 3 Pro 200 plans with 30% spare; you hold 7". | done (d3565b6) |
| 11 | **Cost per use.** Dollars per thread and per percent used, per account and blended per provider. | done (d3565b6) |
| 20 | **Credit balance.** Show Codex purchased credits, what they are worth, and when they get spent. | done (d3565b6) |
| 15 | **Self-check panel.** Monitoring sign-ins about to expire, reconnects needed, stale accounts, door and rclone health, each with its one-click fix. | done (ed8d1f6) |
| 16 | **Telemetry policy.** Local only by default; at most an opt-in crash report inside the existing diagnostics export. Recorded in SECURITY.md and built with item 15. | done (ed8d1f6) |

## Wave 3: done (merged at a153ddc on claude/subscription-value)

| # | Item | Status |
|---|---|---|
| 6 | **An account per project.** Pin a repo to an account through the CLIs' config-folder environment variables, so projects stop fighting over the global login. | done (a153ddc) |
| 7 | **Quick switcher.** A global hotkey opens a type-to-filter list of accounts by email, with headroom. | done (a153ddc) |
| 8 | **Bulk actions.** Connect monitoring for all; remove lapsed accounts once their paid period ends. | done (a153ddc) |
| 9 | **Renewal calendar.** Export renewal dates as an `.ics` feed for Outlook or Google Calendar. | done (520049a) |
| 12 | **When you work.** A heatmap of usage by hour and weekday from local Codex and Claude logs, to size accounts for peak hours. | done (5cabf91) |
| 13 | **Monday digest.** One notification with spend, waste, renewals this week, and accounts that ran out. | done (520049a) |
| 14 | **CSV export** of weekly history. | done (5cabf91) |
| 21 | **Usage by project and model, per account.** Which repos each account worked in, which models it used and how many tokens each, from local Claude Code and Codex logs; attributed to accounts by the thread record (Codex) or by which account was active at the time (Claude). Added by the owner on 2026-10-21. | done (5cabf91) |
| 22 | **Auto-connect monitoring.** When an unmonitored account is the one in use, start its monitoring sign-in automatically (one browser approval), with backoff and a setting. Added by the owner on 2026-10-21. | done (7e7ba16) |
| 19 | **Claude Code status line.** The active account's headroom and time-to-limit in the status line. | done (520049a) |

## Wave 4: done (added by the owner on 2026-10-22; merged at d4c2800, releases on ultra)

| # | Item | Status |
|---|---|---|
| 23 | **Encrypt the account vault** with DPAPI like the monitoring store, with a safe one-time migration and rollback. | done (da3ba6f) |
| 24 | **Green tests and CI**: fix the failing login-script test and the flaky http_diagnostics test; run cargo test on Windows in GitHub Actions for every PR. | done (da3ba6f) |
| 25 | **Token health and revoke-all**: detect revoked sign-ins; one action revokes every SlopBar-held session of an account. | done (da3ba6f) |
| 26 | **Plan change advisor**: concrete cancel/downgrade moves with projected savings and remaining headroom. | done (d4c2800) |
| 27 | **Claude extra usage**: track pay-as-you-go overage (extra_usage limit and credits used) and alert before it bills. | done (d4c2800) |
| 28 | **Spend ledger**: month-by-month cost per plan, year-to-date totals, optional tax rate and currency. | done (d4c2800) |
| 29 | **SlopBar Ultra releases and updates**: signed installers from the fork; Ultra's updater follows the fork's releases. | done (ultra-releases ac7e652) |

## Wave 5: done (merged at 40439db)

| # | Item | Status |
|---|---|---|
| 30 | **Translations** of every new string into the nine other supported languages. | done (40439db) |
| 31 | **Exact Claude attribution**: a Claude Code SessionStart hook recording which account each session started on. | done (40439db) |
| 32 | **Account leases**: agents claim and release an account for a job through the door. | done (40439db) |
| 33 | **Smart launcher**: slopbar-run combines project pins with recommend_account. | done (40439db) |
| 34 | **Sparklines and compact mode** in the flyout. | done (40439db) |
| 35 | **Onboarding** for first run. | done (40439db) |
| 36 | **Accessibility pass**: keyboard, screen reader, contrast. | done (40439db) |

## Wave 6: done (added by the owner on 2026-10-22; merged at 40439db)

| # | Item | Status |
|---|---|---|
| 37 | **Install lock and strict single instance**: a machine-wide install lock; a second instance always hands over and exits; Health flags duplicates. Prompted by two instances running after racing installs on 2026-10-22. | done (40439db) |
| 38 | **MCP server cleanup**: slopbar-mcp exits when its agent disconnects; Health counts lingering servers. | done (40439db) |
| 39 | **Faster refresh**: per-phase timing; non-essential work off the refresh path; under 5 s for 15 accounts. | done (40439db) |
| 40 | **Runaway-agent alert**: abnormal burn per account, with the repo and model behind it. | done (40439db) |
| 41 | **Reset-credit advisor**: spend a banked Codex reset or switch; remind before resets expire. | done (40439db) |
| 42 | **Capacity-shortage advice**: the cheapest fix when the pool is short. | done (40439db) |
| 43 | **Encrypted backup and restore** of settings, plan details and history (never sign-ins). | done (40439db) |
| 44 | **Opt-in phone alerts** through a self-hostable push service (ntfy). | done (40439db) |

## Wave 7: done (added by the owner on 2026-10-22; merged at ac9e355)

| # | Item | Status |
|---|---|---|
| 45 | **Smaller footprint without losing features**: Settings, switcher and flyout built on demand; one shared WebView2 process group; quiet idle timers; capped diagnostics exports and install rollbacks. Measured: 404 MB -> 171 MB private and 4.88% -> 0.73% of a core at idle. | done (07416eb) |
| 46 | **Native taskbar widget**: draw the always-visible widget with Windows' own drawing instead of a web view, for a small (~80-100 MB) idle footprint. | done (ac9e355) |
| 47 | **Flyout on demand**: the flyout is built on click and closed when dismissed, so no WebView2 process runs at idle. Done in the native-widget session. | done (a74cf4b); replaced by 3418c53/d2fe406/7109682: the delay on click was too slow, so the flyout is now built hidden at startup and shown instantly (checked on screen: drawn within 0.3 s of a click) |
| 48 | **Full account tooltips** in the compact flyout and on the native widget (the same detail as the full view), **sparklines fixed** (a key-case mismatch had left them empty), **WSL never started needlessly** (`wsl_accounts` auto/on/off), and **the app no longer quits when the flyout closes**. Upstream thelioo/slopbar#33. | done (4194a17) |
| 49 | **Widget docks in a free gap**: it reads every taskbar button through UI Automation, plus the taskbar's own child windows (the Windows 10 weather, `DynamicContent2`) and docks in the first free span left of the tray, so it never covers the weather; a notice too long for the room is cut with an ellipsis and its tooltip gives the full text. Where no gap fits it squeezes (no margins, then no ring padding) instead of overlapping. Checked on screen on Windows 11 (BACHELOR) and Windows 10 (UltraMagnus) with the weather showing. Upstream thelioo/slopbar#34. | done (3258378, 3ae57af) |
| 30b | **Translate the 130 strings added in waves 5 and 6.** | done (aab0f12) |
| 50 | **Account roster**: opt-in, an encrypted list of each computer's accounts (provider, id, email, plan; never sign-ins) in the in-use sharing folder or rclone remote, under a roster passphrase set on each computer. A new computer's Sign in all accounts queues every account the others have, and the flyout and Health say how many wait. Verified on the installed build: written, read back, a synthetic peer's two accounts queued. Agents can run the whole flow through MCP (update_settings, set_roster_passphrase, sync_roster, get_roster, sign_in_all), so the user only supplies the passphrase and completes the sign-in pages. Not yet turned on. | done (05492b0, a1b8ae7) |
| 51 | **A fresh browser profile per email for sign-ins**: kept per-email profiles collected stale Google sessions, so every sign-in (and each email's Claude and Codex sign-ins in Sign in all accounts) now opens in a brand-new empty profile that is closed and deleted when the email's turn ends; old profiles are deleted at startup, and a Stop button (or closing the browser) ends a run. Checked on screen on Virtual-TEN (Windows 10, Edge) with synthetic emails, nothing typed: clean window, email prefilled, Codex in the same browser, closed and deleted after, a new profile for the next email. Upstream thelioo/slopbar#36. | done (5680411, 869362f, d4c4f0e, ebd4578) |
| 52 | **Bring up another computer from an agent over SSH**: `get_machine_readiness`, `pair_machine` and `run_on_machine` let an agent on a computer with the accounts check a new one (SlopBar running, someone at an unlocked desktop, browser, roster), give it the roster key over the SSH connection's input (generated when none exists, never shown, stored with DPAPI) and start Sign in all accounts on its desktop once the user is at its screen. Door sign-ins are refused while the desktop is locked or not shown. Field guide in `docs/remote-setup.md` (OpenSSH key auth, Tailscale). Run for real: BACHELOR paired with Virtual-TEN. That surfaced two fixes: a pipeline stage so Windows PowerShell waits for slopbar-mcp, and a check that rclone there really has the remote. Signing in all 15 accounts there led to: one browser window per email, opening on a tab that names the account (Chrome showed no profile name), a notification per step, Codex workspace mismatches named as such, a stop when a page never opens (40 s), a still-waiting notice with Retry and Stop (5 min), Retry on the stop notice and in Settings, and each step's outcome in `get_machine_readiness`, which can wait for the next change (`wait_seconds`, `since`) instead of being polled. Upstream thelioo/slopbar#37. | done (2f2b8de, fe17e6d, 3f0bd46, 2d62d44, 3b2dc97, f868dc5, d7dc92e, 6eb2b00) |
| 53 | **One account on several machines flagged**: an account in use on more than one machine at once (live presence only, so a machine that shut down drops out by the presence expiry) gets an amber "In use on N machines" line or badge in the flyout, a chip in Settings > Accounts, and an amber badge on the widget's ring. Tooltips name the machines and say why it matters (limits drain N× as fast, and sessions may be signed out). One notification per overlap, debounced, with a setting to turn it off; `overlap` in get_usage, recommend_account (excludes it and recommends a free account) and get_health. All 10 languages. Released as 0.1.0-ultra.4 and checked in the installed app on BACHELOR and ULTRA-MAGNUS during a live Codex overlap (rendered off-screen through inspect_window, since both desktops were locked). Upstream thelioo/slopbar#41, stacked on #40. | done (5beadba, 9a17ff3; ultra 5b1bdcf) |
| 54 | **Desktop app account beside the CLI's**: Claude Desktop's account is read from the one key it publishes (`lastKnownAccountUuid`; a typed parse skips its token caches); the Codex app shares the Windows CLI's `~/.codex` sign-in. "CLI: A · Desktop: B" in amber in the flyout, Settings > Accounts and the widget tooltip; "Desktop app" joins the in-use places and is shared through presence (hashes only), counting for overlap. `desktop`/`seats` in get_usage, `desktop_drift:<provider>` in get_health. Setting "Use also switches the desktop app" (off by default): no supported way exists to sign either app in from outside (`claude://` links open chats only), so Use opens Claude Desktop, names the account and watches for the change; for Codex it says to restart the app. switch_account returns `desktop`. Review panel (2 Codex, 1 Claude security) found 5 real defects, all fixed. Released as 0.1.0-ultra.5; all three machines updated; checked in the installed app (widget tooltip and Settings show "Windows, Desktop app" here and on ULTRA-MAGNUS / VIRTUAL-TEN through presence; no machine is drifted now, so the drift line was checked through the real UI code with synthetic accounts). Upstream thelioo/slopbar#42, stacked on #41. | done (517b55c, 31800d7; ultra ede8284) |
| 55 | **Machine names wherever an account is shown**: every account names the computers it is in use on, this PC first and marked, then the other computers sharing their status: "In use on DESK-1 (this PC, Desktop app), LAPTOP-2 (WSL: Ubuntu)". In the flyout's and the widget's tooltips, a second line under the name in the compact flyout (rows stay 34 px), the full cards, Settings > Accounts and Subscriptions (chips name the machines; the places are in their tooltip; the chip gives way only after the organization text), the quick switcher (which also finds an account by a machine's name), and the overlap chip and notification, which mark this PC. Nothing new is shared: presence already carried each computer's name, and the windows learn this PC's own from a `this_machine` command. 3 new strings in all 10 languages. Released as 0.1.0-ultra.9, with the Settings chip fix in 0.1.0-ultra.10; checked in the installed app (widget tooltip, a real click on the flyout, Settings, the switcher). Every machine with SlopBar took 0.1.0-ultra.10 by itself within 21 minutes of its release (restarts spread from 07:59 to 08:13 UTC), with no `-UpdateNow`. An adversarial review after release (privacy, best alternative, how this PC is identified) led to 0.1.0-ultra.11: this PC is named only with in-use sharing on; an overlap marks this PC by its place, not by a name a clone may share; a record under this PC's name is not listed twice; the compact line lists named machines only; the agent door counts machine names as untrusted data. | done (e5a2efb, b0b1570, 984fd9a, b231a48; ultra d6c9b7b, 10feee0, ad53f0b) |
| 56 | **The flyout opens without freezing**: while closed, its WebView2 is suspended, so every event sent to it queued and replayed as it opened, and each `usage-updated` meant three renders and about nine IPC calls. While closed it now only notes that usage or settings changed (old notices are dropped); as it opens it renders fresh usage and health once, and reloads sparklines, subscriptions and settings only if it missed a change. Measured on VIRTUAL-TEN with 60 queued updates: busy after opening ~3.9 s → ~0.5 s, slopbar.exe CPU ~25 s → 63 ms, worst UI-thread stall 1,197 ms → 10 ms. Released as 0.1.0-ultra.8. Upstream: queued for the next batch, after the pins fix, while upstream is dormant (thelioo/slopbar#43 was opened and then closed unmerged; its branch `pr/flyout-resume-backlog` stays at 3cfb328). | done (e77cd66, 38a82a2; ultra 34c91ea) |
| 57 | **Sharing set up in one go**: a computer with no sharing location gets **Set up** (Settings → Data → Other computers, and the getting-started guide). SlopBar Ultra uses an installed rclone of v1.60 or later, or downloads rclone v1.76.0 (pinned SHA-256, rclone's own site then GitHub, unpacked with Windows' tar), runs rclone's own Dropbox sign-in without its browser and opens the page itself (rclone's stdout closed, `--no-output`), turns sharing on and lists the computers it sees there; a denied sign-in, a stale page and a busy or reserved port each say what to do. Agents get `setup_sharing` (`confirm: true`, `reconnect`, `cancel`) and `get_sharing_setup`; with `ssh_host` a computer nobody sits at is set up from this one, its Dropbox page opening here through an SSH tunnel to its rclone, so the token goes straight there. An automatic update waits while a setup runs. rclone's client covers the whole Dropbox, which the button says; a Dropbox app with App-folder access needs the owner to register it. | done (5147f15, 460a5e2; ultra: the merge of 460a5e2) |
| 58 | **Commands on another computer run under PowerShell 7**: everything SlopBar Ultra runs over SSH (`pair_machine`, `run_on_machine`, readiness and sharing setup with `ssh_host`, the Retry button) sends its script to `pwsh`, and the same script to Windows PowerShell only where pwsh never started it. The script's first line says it started, so no tool runs twice and Windows PowerShell never starts where PowerShell 7 runs: a 5.1 process started over SSH can outlive its session and keep growing (one reached 85 GiB). Exit codes can't tell this, since a PowerShell remote shell turns every failure into 1. The SSH guide now sets PowerShell 7 as the SSH shell where installed, never Windows PowerShell. Three review rounds (a breaker, an alternatives advocate, a mutation-minded test reviewer); a one-command `cmd` choice on the target was rejected as fragile. Released as 0.1.0-ultra.25 (ultra.24 had the first version); every machine with SlopBar took it by itself within 20 minutes, readiness answered over SSH from this side through pwsh, and no Windows PowerShell was left behind. Upstream: waits with the backlog while upstream is dormant. | done (55c0ab5, e44d8d0, 43e7cdb, c0ecb23; ultra afc8f95, e070294) |
| 59 | **Two computer names in full in the compact flyout**: an account's line under its email runs on under the overlap badge, sparkline and percentage (a grid row: 249 px at the flyout's 380 px, was 110 beside the badge), names two computers in full, this PC second so that a narrow row cuts its mark rather than a name, then how many more ("+1"); two records under one name count once, as the badge does. Rows stay 34 px in every language, with or without the scrollbar; the tooltip still names them all. | done (dd33924, ef3d312; ultra: the merge of ef3d312) |

## Done before this roadmap (branch `claude/subscription-value`; upstream thelioo/slopbar#8–#11, stacked on #6)

Subscriptions page with verdicts and renewal dates; Codex inspection from local history;
renewal dates from the providers and user corrections; ChatGPT Pro tier detection
(`prolite`/`pro`/`promax`); accounts in use on other computers through synced folders or rclone;
`inspect_window`, `list_sharing_locations` and `open_billing_page` doors.
