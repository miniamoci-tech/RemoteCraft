RemoteCraft for macOS
=====================

This disk image contains two command-line programs (universal, Intel + Apple
Silicon):

  remotecraft        - the app: share this screen (host) or view a remote one
  remotecraft-relay  - the rendezvous/relay server (usually run on Linux)

INSTALL
-------
Copy the two files somewhere on your PATH, for example:

  sudo cp remotecraft remotecraft-relay /usr/local/bin/

Because the apps are not signed with an Apple Developer certificate, the first
time you run one macOS will block it. Allow it once with:

  xattr -d com.apple.quarantine /usr/local/bin/remotecraft
  xattr -d com.apple.quarantine /usr/local/bin/remotecraft-relay

USE
---
View a remote host:

  remotecraft view --relay HOST:21118 --id 123456789 --password YOURPASS

Share this Mac's screen:

  remotecraft host --relay HOST:21118 --password YOURPASS

PERMISSIONS (host only)
-----------------------
When this Mac is the host, macOS will ask you to grant, in
System Settings > Privacy & Security:

  - Screen Recording  (to capture the screen)
  - Accessibility     (to let the remote side control mouse & keyboard)

Grant them to the terminal app you launch remotecraft from, then relaunch.
Remote audio capture on a macOS host is not supported yet.

More: https://github.com/miniamoci-tech/RemoteCraft
