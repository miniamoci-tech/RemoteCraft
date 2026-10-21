RemoteCraft for macOS
=====================

This disk image contains (universal, Intel + Apple Silicon):

  RemoteCraft.app    - double-click to open the graphical launcher
  remotecraft        - the same program for command-line use
  remotecraft-relay  - the rendezvous/relay server (usually run on Linux)

EASY WAY (graphical)
--------------------
1. Drag RemoteCraft.app to your Applications folder.
2. Because the app is not signed with an Apple Developer certificate, the
   first time macOS will block it. Right-click the app -> Open -> Open, or
   allow it once in System Settings > Privacy & Security ("Open Anyway").
3. It opens a window: choose "View another computer" or "Share this screen",
   fill in the relay address, ID and password, and connect.

COMMAND LINE (optional)
-----------------------
Copy the plain binaries somewhere on your PATH, for example:

  sudo cp remotecraft remotecraft-relay /usr/local/bin/
  xattr -d com.apple.quarantine /usr/local/bin/remotecraft
  xattr -d com.apple.quarantine /usr/local/bin/remotecraft-relay

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
