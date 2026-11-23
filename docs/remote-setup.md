# Bring up another computer with an agent

A field guide for setting up SlopBar on a second Windows computer from an agent (Claude Code,
Codex or any MCP client) running on a computer that already has your accounts. Nobody types a
roster passphrase; you only sit at the new computer's screen to complete each sign-in page.

What the agent does, in order:

1. `get_machine_readiness` with `ssh_host`: is SlopBar running there, is someone signed in at its
   desktop, is there a browser?
2. `pair_machine` with `ssh_host`: both computers share the account roster with the same key.
3. `run_on_machine` → `get_roster`: the accounts waiting to be signed in there.
4. You sit at the new computer and say so; the agent runs `run_on_machine` → `sign_in_all` with
   `confirm: true`, and guides you email by email.

## 1. Reach the other computer over SSH

The agent uses the `ssh` command with your own keys. Either OpenSSH or Tailscale works.

### OpenSSH (any network you can route over)

On the **new computer** (the one being set up), in an elevated PowerShell:

```powershell
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
Start-Service sshd
Set-Service sshd -StartupType Automatic
# PowerShell 7 as the shell for SSH sessions, where it is installed; otherwise OpenSSH's cmd
# (and an earlier version of this guide's Windows PowerShell setting is removed).
$pwsh = "$env:ProgramFiles\PowerShell\7\pwsh.exe"
if (Test-Path $pwsh) {
  New-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShell -PropertyType String -Force -Value $pwsh
} elseif ((Get-ItemProperty HKLM:\SOFTWARE\OpenSSH -ErrorAction SilentlyContinue).DefaultShell -match 'WindowsPowerShell') {
  Remove-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShell
}
```

SlopBar's commands work from either shell, and run under PowerShell 7 where it is installed,
otherwise under Windows PowerShell. Don't make Windows PowerShell (`powershell.exe`) the SSH shell:
started over SSH, it can outlive the session and keep growing (one reached 85 GiB of memory).
Install PowerShell 7 first if it isn't there (`winget install --id Microsoft.PowerShell`), then
run the block above: SSH then never needs Windows PowerShell on that computer.

On the **computer with the accounts**, create a key once and copy its public half over:

```powershell
ssh-keygen -t ed25519                     # accept the default path; a passphrase is up to you
type $env:USERPROFILE\.ssh\id_ed25519.pub
```

On the new computer, put that line in the right file:

- a standard user: `C:\Users\<you>\.ssh\authorized_keys`
- a member of Administrators: `C:\ProgramData\ssh\administrators_authorized_keys`, then
  `icacls C:\ProgramData\ssh\administrators_authorized_keys /inheritance:r /grant Administrators:F /grant SYSTEM:F`

Give it a name on the computer with the accounts, in `C:\Users\<you>\.ssh\config`:

```
Host newpc
    HostName 192.168.1.42        # or its DNS name
    User <your Windows user name there>
```

Then connect once by hand so the host key is recorded (the agent never accepts an unknown host
key) and check that key auth works without a prompt:

```powershell
ssh newpc hostname
ssh -o BatchMode=yes newpc hostname
```

### Tailscale

Install Tailscale on both computers and sign in to the same tailnet. Use the new computer's
Tailscale name (`newpc` or `newpc.tailnet-name.ts.net`) as `HostName` above, or directly as
`ssh_host`. Windows has no Tailscale SSH server, so keep OpenSSH Server on the new computer as above;
Tailscale only gives the two computers a private route to each other wherever they are.

## 2. On the new computer, once, at its screen

- Install SlopBar and start it from the Start menu. Agents never start it over SSH: started from
  an SSH session it would run where nobody can see it.
- Sharing through Dropbox: click **Set up** in Settings → Data → Other computers here, or let the
  agent start it from the computer with the accounts (below); either way Dropbox's page opens here,
  for you to sign in and click Allow. With a synced folder instead (OneDrive, Dropbox…), sign in to
  it here and choose it in Settings → Data → Other computers. Pairing points SlopBar at the same
  rclone remote by name.
- Leave Windows signed in. Locking is fine until the sign-ins start.

## 3. Point the agent at it

Register SlopBar's MCP server with your agent on the computer with the accounts (see "Agent
access" in the README), then ask, for example:

> Set up SlopBar on newpc over SSH and sign in my accounts there.

The agent should:

1. Call `get_machine_readiness` with `ssh_host: "newpc"`. `problems` names anything in the way:
   `no_desktop_session` (nobody is signed in at its desktop), `app_not_running`, `desktop_locked`,
   `desktop_not_shown` (a closed Remote Desktop or another user switched in), `no_browser`. Two
   codes never stop sign-ins but say SSH starts Windows PowerShell there: `ssh_shell_windows_powershell`
   (its SSH shell is set to it: run the OpenSSH block above there) and `no_pwsh_over_ssh`
   (PowerShell 7 did not start there, so SlopBar used it: install PowerShell 7 there).
2. If the new computer has no sharing location yet (readiness lists `no_sharing_location` or
   `rclone_remote_missing`), ask you to sit at its screen, then call `setup_sharing` with
   `ssh_host: "newpc"` and `confirm: true`. SlopBar there gets rclone (a fixed, checksum-verified
   download) and opens Dropbox's page in its own browser: sign in there with the Dropbox account
   this computer uses and click Allow. Follow it with `get_sharing_setup` and the same `ssh_host`
   (`wait_seconds: 45` with `since`) until `phase` is `done`; `sees_this_computer: false` means it
   is signed in to another Dropbox account (run it again with `reconnect: true`). If that screen is
   out of reach, `open_here: true` opens the page in this computer's browser instead, through an
   SSH tunnel; the token still goes straight to the new computer's rclone.
3. Call `pair_machine` with `ssh_host: "newpc"`. It uses this computer's roster passphrase (or
   generates a 256-bit key when there is none), sends it over the SSH connection's input to the
   new computer's SlopBar, which saves it with Windows DPAPI as it saves any roster passphrase,
   and syncs both rosters. The key never appears in the tool's arguments, results, errors or logs.
   Nothing changes on either computer until the new one has shown it can share where this one
   does. If the new computer already has a different roster passphrase, pairing refuses unless you
   tell the agent to replace it (`replace_key: true`).
4. Call `run_on_machine` with `tool: "get_roster"` to list the accounts waiting there.
5. Ask you to sit at the new computer's screen, and wait until you say you are there.
6. Call `run_on_machine` with `tool: "sign_in_all"` and `arguments: { "confirm": true }`. Sign-in
   pages open on the new computer's desktop, an email at a time in a fresh browser profile. You
   complete every page; the agent never signs in or approves anything. If the desktop is locked or
   not shown, nothing starts and the agent hears why.
7. Follow progress with `run_on_machine` → `get_roster` or `get_usage`.

## Every month: renew every computer's sign-ins from one desk

Claude ends every sign-in about 30 days after it is made, on every computer. Once a computer is
paired or checked from here (or named by the shared account list and reachable at its name in
`~/.ssh/config`), its renewals can be made from whichever computer you sit at: Settings → Accounts →
**Sign in all your computers from here**, or ask the agent there to call `sign_in_all` with
`everywhere: true` (it shows the plan; with `confirm: true` it raises a notification you start
yourself, and opens the same confirmation as the button). Each other computer runs its CLI's
sign-in with no window and hands its page over; the page opens in this computer's browser, and the
provider's answer goes back to that computer's CLI through an SSH tunnel for the one port that page
names. Their screens can stay locked. PowerShell 7 must start over SSH on each of them. A computer is
left out, and named with the reason, when SSH can't reach it, SlopBar isn't running there (nobody
signed in to Windows), its SlopBar is older (it updates itself), a sign-in is already open there, or
its SSH server starts Windows PowerShell (run the OpenSSH block above there).

## Without an MCP client

`slopbar-mcp.exe` runs one tool from a shell and prints one JSON line (`{"ok": …}` or
`{"error": "…"}`):

```powershell
& "$env:LOCALAPPDATA\SlopBar\slopbar-mcp.exe" --call get_machine_readiness ssh_host=newpc | Out-String
& "$env:LOCALAPPDATA\SlopBar\slopbar-mcp.exe" --call pair_machine ssh_host=newpc | Out-String
& "$env:LOCALAPPDATA\SlopBar\slopbar-mcp.exe" --call run_on_machine ssh_host=newpc tool=get_roster | Out-String
```

Keep the pipe: PowerShell neither waits for this windowless program nor shows what it prints
unless its output is captured, so without one each line returns at once and shows nothing.

## Troubleshooting

| Error | Fix |
| --- | --- |
| SSH could not run the command (Permission denied) | The public key is not in the right `authorized_keys` file, or that file's permissions are too open (administrators: the `icacls` line above). |
| SSH could not run the command (Host key verification failed) | Connect once by hand with `ssh newpc` and accept the host key. |
| SlopBar is not running on the other computer's desktop | Start it there from the Start menu. |
| The other computer has no working rclone remote named … (or readiness lists `rclone_remote_missing`) | At its screen, click **Set up** in Settings → Data → Other computers, or have the agent run `setup_sharing` with its `ssh_host` and `confirm: true`: either way the Dropbox sign-in opens there. |
| `setup_sharing` with `ssh_host` fails with `desktop_locked` or `desktop_not_shown` | Nobody can see the new computer's desktop: unlock it (or go back to it), then try again. |
| `setup_sharing` with `ssh_host` and `open_here` fails with `tunnel_failed` | Port 53682 is in use on this computer (another rclone sign-in is waiting), or SSH could not connect. Close the other sign-in and try again. |
| `setup_sharing` with `ssh_host` fails with `update_slopbar_there` | The other computer's SlopBar predates the guided setup: update it there. |
| The other computer answered without a SlopBar result | Update SlopBar there; pairing needs the same version on both. |
| Readiness lists `ssh_shell_windows_powershell` | That computer's SSH shell is Windows PowerShell: run the OpenSSH block above there, in an elevated PowerShell (it sets PowerShell 7, or removes the setting so OpenSSH uses cmd). |
| Readiness lists `no_pwsh_over_ssh` | PowerShell 7 did not start there over SSH, so SlopBar used Windows PowerShell: install PowerShell 7 there (`winget install --id Microsoft.PowerShell`). |
