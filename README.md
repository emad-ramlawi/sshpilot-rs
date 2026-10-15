<p align="center">
  <img src="data/icons/io.github.emad_ramlawi.SshPilotRs.svg" width="112" alt="SSH Pilot RS icon">
</p>

<h1 align="center">SSH Pilot RS</h1>

<p align="center">
  A simple, fast SSH connection manager for Linux, written in Rust.<br>
  Your <code>~/.ssh/config</code> hosts in groups you sort by hand, with a built-in terminal and SFTP file manager.
</p>

---

## What it does

- **Uses your `~/.ssh/config`.** Every `Host` in it shows up in the sidebar. Addresses, users, ports, keys, `ProxyJump`, agents: all of it is plain OpenSSH, so whatever works with `ssh myhost` works here. Save the file and the list updates.
- **Groups and manual sorting.** Drag connections up and down, into and out of groups, and drag groups to reorder them. The order you set is the order you keep.
- **Terminal tabs.** Double-click a host to open a terminal running `ssh` in a tab.
- **File manager.** Right-click a host and choose **Open Files** to browse it over SFTP. Upload (button or drag files in), download, new folder, rename, delete. If the host needs a password, open a terminal to it first: the files tab reuses that login.
- **Few buttons.** One **+** in the sidebar (new connection, new group); everything else lives in the right-click menu.

## Install

Download the `.tar.gz` for your machine from the [Releases page](https://github.com/emad-ramlawi/sshpilot-rs/releases): `linux-amd64` for Intel/AMD, `linux-arm64` for ARM (Raspberry Pi 4/5, Ampere, Snapdragon laptops).

```sh
tar xzf sshpilot-rs-*-linux-amd64.tar.gz
sudo install -m 755 sshpilot-rs-*-linux-amd64/sshpilot-rs /usr/local/bin/

# optional: app menu entry and icon
sudo cp -r sshpilot-rs-*-linux-amd64/share/. /usr/local/share/
```

That's it. Run `sshpilot-rs`. To uninstall, delete `/usr/local/bin/sshpilot-rs` (and the two files under `/usr/local/share` if you copied them).

The binary uses your system's GTK libraries and `ssh`. Most desktops already have them; if not:

| Distribution | Command |
| --- | --- |
| Debian 13+, Ubuntu 24.04+ | `sudo apt install libgtk-4-1 libadwaita-1-0 libvte-2.91-gtk4-0 openssh-client` |
| Fedora 40+ | `sudo dnf install gtk4 libadwaita vte291-gtk4 openssh-clients` |
| Arch, CachyOS, Manjaro | `sudo pacman -S gtk4 libadwaita vte4 openssh` |

Requires GTK ≥ 4.12, libadwaita ≥ 1.5, and VTE (GTK 4 build) ≥ 0.76.

## Where things are stored

| What | Where |
| --- | --- |
| Hosts | `~/.ssh/config`. **New Connection** only *appends* a `Host` block; the app never rewrites or reorders your file. |
| Groups and order | `~/.config/sshpilot-rs/layout.json` (aliases only, mode `0600`) |
| Passwords | Nowhere. OpenSSH asks for them in the terminal. |

## Why a Rust rewrite

This project is inspired by [sshPilot](https://github.com/mfat/sshpilot) by mfat. sshPilot has a lot of features; I wanted a much smaller app whose bugs I could fix and ship on my own schedule. It was written from scratch: no code is shared, and the artwork is new.

What Rust brings here:

- **Memory safety without a garbage collector.** Buffer overflows, use-after-free and data races, the bug classes behind most security advisories in C/C++ code, are ruled out at compile time in this codebase.
- **One small native binary.** About 1 MB, with no Python interpreter or pip packages to install. Copy it to `/usr/local/bin` and run it.
- **Fast and light.** No interpreter startup and no background daemon. SSH and SFTP run on a small async runtime (Tokio) next to the UI thread, so a slow server never freezes the window.
- **Checked dependencies.** Dependencies are pinned in `Cargo.lock` and checked against the [RustSec advisory database](https://rustsec.org/) with `cargo audit` in CI.

To be fair to C: the GUI toolkit (GTK, libadwaita, VTE) and `ssh` are still the system's own C programs. You get those from your distribution, with its security updates.

## Security design

- **No daemon, no listening sockets, no telemetry, no update checker.** One app process, plus an `ssh` process for each open tab.
- **No stored secrets.** Authentication is entirely OpenSSH: keys, agent, `known_hosts`, prompts in the terminal.
- **No shell.** `ssh` is started with an argument list, never through `sh -c`, and the alias always follows `--`, so it can never be read as an option (blocks `-oProxyCommand=…` style injection). New entries are validated before they are written.
- **SFTP never prompts** (`BatchMode=yes`). It reuses the terminal's connection through a multiplexing socket in a private `0700` folder under `$XDG_RUNTIME_DIR`, closed when the tabs close (`ControlPersist=no`).
- **Hostile servers can't write outside your folder.** Remote file names containing `/`, `..` or NUL are ignored, downloads never overwrite an existing file, and recursive delete never follows symlinks.
- **Private files.** The layout file is written atomically with mode `0600`.

## Build from source

Needs Rust ≥ 1.88 and the GTK development packages (`libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev` on Debian/Ubuntu; `gtk4 libadwaita vte4` on Arch).

```sh
cargo build --release          # binary: target/release/sshpilot-rs
cargo test                     # unit tests
scripts/package.sh             # dist/sshpilot-rs-<version>-linux-<arch>.tar.gz for this machine
scripts/dist.sh                # amd64 + arm64 tarballs, built in Ubuntu 24.04 containers (podman/docker)
```

## License

GPL-3.0-or-later, the same license as sshPilot. See [LICENSE](LICENSE).

Not affiliated with or endorsed by the sshPilot project.
