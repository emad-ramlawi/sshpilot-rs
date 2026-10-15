# Changelog

## 0.2.2

- Clicking the app icon in the top-left corner opens a new local terminal (when your desktop shows the window icon there, as KDE does by default).

## 0.2.1

- Starts maximized; <kbd>F11</kbd> switches full screen on and off.

## 0.2.0

Faster, steadier for long sessions, and the look of sshPilot.

**New**
- File manager with your files and the server's side by side: Name / Size / Modified columns, color file-type icons, Upload and Download buttons, a hidden-files toggle, and a progress bar with speed.
- Terminal font picker (monospace fonts, applied as you choose) and text size (− / + or <kbd>Ctrl</kbd>+<kbd>+</kbd> / <kbd>-</kbd> / <kbd>0</kbd>), both remembered.
- Local terminals: one opens at launch, more with <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd>. They run your login shell (bash, zsh, fish, …), even right after `chsh`.
- Copy and paste with <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>C</kbd> / <kbd>V</kbd>.
- Tab right-click menu: Duplicate, Open Files, Close.
- The connections panel folds away (arrow or <kbd>F9</kbd>) and stays that way.
- Hosts from files that `~/.ssh/config` `Include`s.
- `sshpilot-rs --install` adds the menu entry and icon (needed for the right icon on Wayland); `--uninstall`, `--version`.
- Looks like sshPilot: white on black with the Tango palette, tabs in the title bar, roomy sidebar with color folder and server icons.

**Faster**
- Uploads about 12× faster on a 20 ms link (5 → 66 MB/s): writes fill OpenSSH's channel window.
- Tabs to one host share a login that stays up 10 minutes after the last tab closes, so reopening is instant.

**Fixed**
- Closing a tab no longer cuts the other tabs to the same host.
- Long idle sessions: keep-alives hold them open, and the file manager reconnects by itself after a drop.
- A second launch raises the open window instead of starting another that overwrote the same layout file.
- An unreadable `layout.json` is kept as `layout.json.broken` instead of being overwritten.
- Uploads can never replace a remote file (exclusive create); failed transfers leave no partial file.
- Double-clicking a symlinked file downloads it; rename and delete act on the folder that was listed.

**Releases** are now plain executables with a `.sha256` next to each, instead of tarballs.

## 0.1.0

First release: your `~/.ssh/config` hosts in groups you sort by hand, terminal tabs running the system `ssh`, and an SFTP file manager that reuses the terminal's login.
