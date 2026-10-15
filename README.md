<p align="center">
  <img src="data/icons/io.github.emad_ramlawi.SshPilotRs.svg" width="112" alt="SSH Pilot RS icon">
</p>

<h1 align="center">SSH Pilot RS</h1>

<p align="center">
  A simple, fast SSH connection manager for Linux, written in Rust.<br>
  Your <code>~/.ssh/config</code> hosts in groups you sort by hand, with a built-in terminal and SFTP file manager.
</p>

---

## Features

- **Your `~/.ssh/config`, as is.** Every `Host` (including `Include`d files) appears in the sidebar and updates when you save the file. It's plain OpenSSH, so keys, agents and `ProxyJump` work as they do with `ssh`.
- **Groups you sort by hand.** Drag hosts and groups into any order; it's kept. Fold the panel away with the arrow or <kbd>F9</kbd>.
- **Terminal tabs.** Double-click a host for an `ssh` tab. Local terminals run your login shell (bash, zsh, fish…); one opens at launch, more from **+**, <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd> or the app icon in the top-left corner.
  - Right-click a tab to **Duplicate** it (a local copy starts in the same folder) or **Open Files**.
  - A tab closes when its session ends; if `ssh` fails, it stays open showing the error.
- **File manager.** Right-click a host → **Open Files**: your files left, the server's right (name, size, date; the eye shows hidden files), with **Upload** / **Download**, drag and drop from the desktop, a progress bar with speed, and new folder / rename / delete. It reuses the terminal's login, so for password hosts open a terminal first.
- **sshPilot's look.** White on black (Tango palette), tabs in the title bar, color file icons. Font and size from the **Aa** button (defaults to your sshPilot font, else `Monospace 12`); zoom with − / +.
- **Built for long sessions.** Tabs to one host share a single login, so new tabs open instantly. Keep-alives survive idle NAT and firewall timeouts, and the file manager reconnects on its own.

### Keyboard shortcuts

| Keys | Does |
| --- | --- |
| <kbd>Ctrl</kbd>+<kbd>N</kbd> | New connection (adds a `Host` to `~/.ssh/config`) |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd> | New local terminal |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>C</kbd> / <kbd>V</kbd> | Copy / paste in a terminal |
| <kbd>Ctrl</kbd>+<kbd>+</kbd> / <kbd>-</kbd> / <kbd>0</kbd> | Larger / smaller / default terminal text |
| <kbd>F9</kbd> | Fold or show the connections panel |
| <kbd>F11</kbd> | Full screen on or off (the app starts maximized) |
| <kbd>Enter</kbd> or double-click | Open a terminal to the selected host; in the file manager, open a folder or send a file across |

## Install

Get the binary for your machine from [Releases](https://github.com/emad-ramlawi/sshpilot-rs/releases): `linux-amd64` (Intel/AMD) or `linux-arm64` (Raspberry Pi 4/5, Ampere, Snapdragon laptops), each with a `.sha256`.

```sh
sha256sum -c sshpilot-rs-*-linux-amd64.sha256      # optional: check the download
install -Dm 755 sshpilot-rs-*-linux-amd64 ~/.local/bin/sshpilot-rs
~/.local/bin/sshpilot-rs --install                 # app menu entry and icon
```

- **`--install`** writes a `.desktop` entry and the icon under `~/.local/share` (nothing is downloaded). Wayland needs them to show the right window icon. For all users, put the binary in `/usr/local/bin` and have each user run `--install`.
- **Update:** replace the binary at the same path; no need to rerun `--install`.
- **Uninstall:** `sshpilot-rs --uninstall`, then delete the binary.

It uses your system's GTK, libadwaita, VTE and `ssh` (GTK ≥ 4.12, libadwaita ≥ 1.5, VTE GTK 4 ≥ 0.76). If any are missing:

| Distribution | Command |
| --- | --- |
| Debian 13+, Ubuntu 24.04+ | `sudo apt install libgtk-4-1 libadwaita-1-0 libvte-2.91-gtk4-0 openssh-client` |
| Fedora 40+ | `sudo dnf install gtk4 libadwaita vte291-gtk4 openssh-clients` |
| Arch, CachyOS, Manjaro | `sudo pacman -S gtk4 libadwaita vte4 openssh` |

## Where things are stored

| What | Where |
| --- | --- |
| Hosts | `~/.ssh/config`. **New Connection** only *appends* a `Host` block; the app never rewrites or reorders your file. |
| Groups, order, terminal font and size, panel folded | `~/.config/sshpilot-rs/layout.json` (aliases only, mode `0600`) |
| Passwords | Nowhere. OpenSSH asks for them in the terminal. |
| Shared connections, icon copies | `$XDG_RUNTIME_DIR/sshpilot-rs/` (private `0700` folder in memory, cleared at logout) |
| Menu entry and icon | `~/.local/share/applications/` and `~/.local/share/icons/`, only if you run `--install` |
| Read, never written | `~/.config/sshpilot/config.json`, if you used sshPilot: only its terminal font, as the default until you pick one |

## Security

- **Nothing extra running or stored.** No daemon, listening sockets, telemetry or update checks. Authentication is all OpenSSH (keys, agent, `known_hosts`); no secrets are saved.
- **No shell injection.** `ssh` gets an argument list (never `sh -c`) with the alias after `--`, so `-oProxyCommand=…` tricks can't work. New entries are validated before they're written.
- **Safe connection sharing.** The file manager never prompts (`BatchMode=yes`); it reuses the terminal's connection through a socket in your private runtime folder. If `$XDG_RUNTIME_DIR` isn't really private (yours, `0700`, not a symlink, e.g. a shared `/tmp`), sharing is turned off instead. The shared connection outlives single tabs, closes 10 minutes after the last one (`ControlPersist=10m`) or when the app quits, and keep-alives (`ServerAliveInterval=30`, `ServerAliveCountMax=4`) catch a dead link within about two minutes.
- **Hostile servers stay contained.** Remote names with `/`, `..` or NUL are ignored. Downloads never overwrite local files, uploads never replace remote ones (exclusive create), failed transfers leave nothing behind, and recursive delete never follows symlinks.
- **Private files.** `layout.json` is written atomically with mode `0600`.
- **Checked supply chain.** Dependencies are locked and audited against [RustSec](https://rustsec.org/) in CI; GitHub Actions are pinned to exact commits and only the publish step can write to the repo.

## Why Rust

Inspired by [sshPilot](https://github.com/mfat/sshpilot) by mfat, written from scratch as a much smaller app (no shared code, new artwork). Rust rules out memory-safety bugs such as buffer overflows and use-after-free at compile time, and builds one native binary of about 1.3 MB with no interpreter. SSH and SFTP run on a small async runtime (Tokio), so a slow server never freezes the window. GTK, libadwaita, VTE and `ssh` are still your distribution's C libraries, with its security updates.

## Build from source

Needs Rust ≥ 1.88 and the GTK development packages (`libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev` on Debian/Ubuntu; `gtk4 libadwaita vte4` on Arch).

```sh
cargo build --release          # binary: target/release/sshpilot-rs
cargo test                     # unit tests
scripts/install.sh             # build, install to ~/.local/bin, add menu entry and icon (--uninstall to undo)
scripts/package.sh             # dist/sshpilot-rs-<version>-linux-<arch> (+ .sha256) for this machine
scripts/dist.sh                # amd64 + arm64 binaries, built in Ubuntu 24.04 containers (podman/docker)
```

Two tests need an SSH server you can log in to with a key:

```sh
export SSHPILOT_RS_TEST_HOST=myhost SSHPILOT_RS_TEST_SSH_CONFIG=~/.ssh/config
cargo test -- --ignored sftp_round_trip                        # upload, download, rename, delete
cargo test --release -- --ignored --nocapture sftp_speed       # transfer speed, tuned vs. library defaults
```

**Releasing:** add notes to [CHANGELOG.md](CHANGELOG.md), bump `version` in `Cargo.toml`, push a `v<version>` tag. GitHub Actions builds both binaries and publishes them with those notes.

## License

GPL-3.0-or-later, the same license as sshPilot. See [LICENSE](LICENSE). Not affiliated with or endorsed by the sshPilot project.
