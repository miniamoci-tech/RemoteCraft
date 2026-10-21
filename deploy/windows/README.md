# Running the RemoteCraft relay on Windows (office LAN)

Use this when the viewer reaches the office network over a VPN, so the relay
only needs to be reachable inside the LAN. The relay is a single
`remotecraft-relay.exe` with no runtime dependencies.

## 1. Get the binary

Build it (see the repository root README) or copy a prebuilt
`remotecraft-relay.exe`. Put it somewhere stable, e.g.
`C:\RemoteCraft\remotecraft-relay.exe`.

## 2. Allow the port through Windows Firewall

In an **Administrator** PowerShell:

```powershell
New-NetFirewallRule -DisplayName "RemoteCraft relay" -Direction Inbound `
  -Action Allow -Protocol TCP -LocalPort 21118
```

## 3. Run it at startup as a background service

Using the built-in Task Scheduler (no extra tools), in an Administrator
PowerShell:

```powershell
$action  = New-ScheduledTaskAction -Execute "C:\RemoteCraft\remotecraft-relay.exe" -Argument "--bind 0.0.0.0:21118"
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
Register-ScheduledTask -TaskName "RemoteCraftRelay" -Action $action -Trigger $trigger -Principal $principal
Start-ScheduledTask -TaskName "RemoteCraftRelay"
```

Check it is listening:

```powershell
netstat -ano | findstr 21118
```

To stop/remove it:

```powershell
Stop-ScheduledTask -TaskName "RemoteCraftRelay"
Unregister-ScheduledTask -TaskName "RemoteCraftRelay" -Confirm:$false
```

## Notes

- The two desktop peers must be configured with this PC's LAN address (e.g.
  `192.168.1.50:21118`). Give the office PC a static/reserved IP so the address
  doesn't change.
- The relay forwards only opaque, end-to-end-encrypted bytes and stores nothing.
