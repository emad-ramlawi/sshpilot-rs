//! Hosts come from `~/.ssh/config`; this app only remembers how you arranged
//! them (groups and the manual order: the order of the vectors) and a few
//! view settings (terminal font and text size, the folded panel).

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub type Id = u64;

/// One concrete `Host` alias and what its block says, for display only.
/// ssh itself always reads the real config.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Host {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<String>,
}

impl Host {
    /// `user@hostname:port`, leaving out what the config does not set.
    pub fn target(&self) -> String {
        let mut text = self.hostname.clone().unwrap_or_else(|| self.alias.clone());
        if let Some(user) = &self.user {
            text = format!("{user}@{text}");
        }
        if let Some(port) = &self.port {
            text = format!("{text}:{port}");
        }
        text
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn ssh_config_path() -> PathBuf {
    home_dir().join(".ssh").join("config")
}

/// An alias ssh will take as a destination and never as an option.
fn usable_alias(alias: &str) -> bool {
    !alias.is_empty()
        && !alias.starts_with('-')
        && !alias.contains(['*', '?', '!', '"', '#'])
        && !alias.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// What `load_ssh_config` found.
pub struct SshConfig {
    pub hosts: Vec<Host>,
    /// Files read and folders searched, to reload when they change.
    pub watch: Vec<PathBuf>,
}

/// ssh's limit on nested `Include`s.
const MAX_INCLUDE_DEPTH: usize = 16;

/// The hosts of the config file at `path` and of the files it includes.
pub fn load_ssh_config(path: &Path) -> SshConfig {
    let mut parser = Parser {
        include_dir: path.parent().map(Path::to_owned),
        ..Parser::default()
    };
    parser.file(path);
    SshConfig {
        hosts: parser.hosts,
        watch: parser.watch,
    }
}

/// Concrete `Host` aliases in file order (wildcard patterns are skipped).
/// `Include` is ignored; see `load_ssh_config`.
#[cfg(test)]
pub fn parse_ssh_config(text: &str) -> Vec<Host> {
    let mut parser = Parser::default();
    parser.text(text);
    parser.hosts
}

#[derive(Default)]
struct Parser {
    hosts: Vec<Host>,
    /// Indexes into `hosts` that the current block applies to.
    current: Vec<usize>,
    /// Where relative `Include` paths start (`None`: ignore `Include`).
    include_dir: Option<PathBuf>,
    /// Files being read, outermost first, to stop include loops.
    stack: Vec<PathBuf>,
    watch: Vec<PathBuf>,
}

impl Parser {
    fn file(&mut self, path: &Path) {
        if !self.watch.iter().any(|p| p == path) {
            self.watch.push(path.to_owned());
        }
        let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        if path.is_dir() || self.stack.len() > MAX_INCLUDE_DEPTH || self.stack.contains(&real) {
            return;
        }
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        self.stack.push(real);
        self.text(&text);
        self.stack.pop();
    }

    fn text(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (keyword, rest) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
                Some((k, rest)) => (
                    k,
                    rest.trim_start_matches(|c: char| c.is_whitespace() || c == '='),
                ),
                None => (line, ""),
            };
            let value = rest.split('#').next().unwrap_or("").trim();
            match keyword.to_ascii_lowercase().as_str() {
                "host" => {
                    self.current.clear();
                    for alias in value.split_whitespace() {
                        if !usable_alias(alias) || self.hosts.iter().any(|h| h.alias == alias) {
                            continue;
                        }
                        self.current.push(self.hosts.len());
                        self.hosts.push(Host {
                            alias: alias.to_owned(),
                            ..Default::default()
                        });
                    }
                }
                "match" => self.current.clear(),
                "include" => {
                    let Some(dir) = self.include_dir.clone() else {
                        continue;
                    };
                    // Lines after the Include belong to the block it is in again.
                    let block = self.current.clone();
                    for pattern in value.split_whitespace() {
                        for path in expand_include(pattern, &dir, &mut self.watch) {
                            self.file(&path);
                        }
                    }
                    self.current = block;
                }
                key @ ("hostname" | "user" | "port") if !value.is_empty() => {
                    for &i in &self.current {
                        // First value wins, as in ssh.
                        let host = &mut self.hosts[i];
                        let slot = match key {
                            "hostname" => &mut host.hostname,
                            "user" => &mut host.user,
                            _ => &mut host.port,
                        };
                        slot.get_or_insert_with(|| value.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
}

/// The files an `Include` pattern names, like ssh's glob: `~` is the home
/// folder, relative paths start in `dir`, and `*`/`?` match within one path
/// component (not a leading dot). Folders searched are added to `watch`.
fn expand_include(pattern: &str, dir: &Path, watch: &mut Vec<PathBuf>) -> Vec<PathBuf> {
    let pattern = match pattern.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home_dir().join(rest.trim_start_matches('/'))
        }
        _ => dir.join(pattern),
    };
    let mut found = vec![PathBuf::new()];
    for part in pattern.components() {
        let part = part.as_os_str();
        let Some(glob) = part.to_str().filter(|p| p.contains(['*', '?'])) else {
            found.iter_mut().for_each(|path| path.push(part));
            continue;
        };
        let mut next = Vec::new();
        for folder in &found {
            if !watch.contains(folder) {
                watch.push(folder.clone());
            }
            let Ok(entries) = fs::read_dir(folder) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|name| {
                    (glob.starts_with('.') || !name.starts_with('.')) && wildcard(glob, name)
                })
                .collect();
            names.sort();
            next.extend(names.into_iter().map(|name| folder.join(name)));
        }
        found = next;
    }
    found
}

/// Whether `text` matches `pattern`, where `*` is any run of characters and
/// `?` any one character.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    // Last `*` seen and where in `text` it started matching.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            // Let that `*` take one more character and retry.
            star = Some((sp, st + 1));
            pi = sp + 1;
            ti = st + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// The fields of a new `Host` block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewHost {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: String,
}

impl NewHost {
    pub fn validate(&self) -> Result<()> {
        if !usable_alias(self.alias.trim()) {
            bail!("Name must be one word without * ? ! # or a leading -");
        }
        let plain = |s: &str| {
            !s.starts_with('-')
                && !s
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == '#' || c == '"')
        };
        let hostname = self.hostname.trim();
        if hostname.is_empty() || !plain(hostname) {
            bail!("Address must be a host name or IP address");
        }
        if !plain(self.user.trim()) || self.user.contains('@') {
            bail!("User contains characters ssh does not accept");
        }
        let port = self.port.trim();
        if !port.is_empty() && !matches!(port.parse::<u16>(), Ok(1..)) {
            bail!("Port must be between 1 and 65535");
        }
        Ok(())
    }

    fn block(&self) -> String {
        let mut block = format!(
            "\nHost {}\n    HostName {}\n",
            self.alias.trim(),
            self.hostname.trim()
        );
        if !self.user.trim().is_empty() {
            block += &format!("    User {}\n", self.user.trim());
        }
        if !self.port.trim().is_empty() {
            block += &format!("    Port {}\n", self.port.trim());
        }
        block
    }
}

/// Append a `Host` block to the ssh config. Appending (instead of rewriting)
/// keeps the file's owner, permissions, symlink and everything already in it.
pub fn append_host(path: &Path, host: &NewHost) -> Result<()> {
    host.validate()?;
    let existing = fs::read_to_string(path).unwrap_or_default();
    if load_ssh_config(path)
        .hosts
        .iter()
        .any(|h| h.alias == host.alias.trim())
    {
        bail!("{} is already in {}", host.alias.trim(), path.display());
    }
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let mut text = host.block();
    if !existing.is_empty() && !existing.ends_with('\n') {
        text.insert(0, '\n');
    }
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Cannot write {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Group {
    pub id: Id,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// Groups and manual order of host aliases.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Layout {
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub ungrouped: Vec<String>,
    #[serde(default)]
    next_id: Id,
    /// Terminal text size, 1.0 = the system's monospace font size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_scale: Option<f64>,
    /// Terminal font, as Pango names it ("Hack 12"); `None` for the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_font: Option<String>,
    /// The connections panel is folded away.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sidebar_hidden: bool,
}

impl Layout {
    pub fn group(&self, gid: Id) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == gid)
    }

    pub fn group_mut(&mut self, gid: Id) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.id == gid)
    }

    fn list_mut(&mut self, group: Option<Id>) -> Option<&mut Vec<String>> {
        match group {
            None => Some(&mut self.ungrouped),
            Some(gid) => self.group_mut(gid).map(|g| &mut g.hosts),
        }
    }

    /// Which group holds `alias` (`None` = ungrouped) and at what index.
    pub fn locate(&self, alias: &str) -> Option<(Option<Id>, usize)> {
        if let Some(i) = self.ungrouped.iter().position(|a| a == alias) {
            return Some((None, i));
        }
        self.groups.iter().find_map(|g| {
            g.hosts
                .iter()
                .position(|a| a == alias)
                .map(|i| (Some(g.id), i))
        })
    }

    /// Match the layout to the config: new aliases go to the end of the
    /// ungrouped list, aliases no longer in the config are dropped.
    /// Returns whether anything changed.
    pub fn reconcile(&mut self, hosts: &[Host]) -> bool {
        let before = self.clone();
        let present = |alias: &String| hosts.iter().any(|h| &h.alias == alias);
        let mut seen = std::collections::HashSet::new();
        self.ungrouped
            .retain(|a| present(a) && seen.insert(a.clone()));
        for group in &mut self.groups {
            group.hosts.retain(|a| present(a) && seen.insert(a.clone()));
        }
        for host in hosts {
            if !seen.contains(&host.alias) {
                self.ungrouped.push(host.alias.clone());
            }
        }
        *self != before
    }

    /// Place `alias` at `index` of `group` (an index into the list as it is
    /// before the move, which is what a drop target points at).
    pub fn move_host(&mut self, alias: &str, group: Option<Id>, index: usize) -> Result<()> {
        self.list_mut(group).context("Unknown group")?;
        let (from, old) = self.locate(alias).context("Unknown host")?;
        let alias = self.list_mut(from).expect("located").remove(old);
        let index = if from == group && old < index {
            index - 1
        } else {
            index
        };
        let list = self.list_mut(group).expect("checked above");
        list.insert(index.min(list.len()), alias);
        Ok(())
    }

    pub fn add_group(&mut self, name: &str) -> Result<Id> {
        let name = name.trim();
        if name.is_empty() {
            bail!("Group name is required");
        }
        // Above every id in use, even if the file was edited by hand.
        let used = self.groups.iter().map(|g| g.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(used) + 1;
        let id = self.next_id;
        self.groups.push(Group {
            id,
            name: name.to_owned(),
            collapsed: false,
            hosts: vec![],
        });
        Ok(id)
    }

    pub fn rename_group(&mut self, gid: Id, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("Group name is required");
        }
        self.group_mut(gid).context("Unknown group")?.name = name.to_owned();
        Ok(())
    }

    /// Delete a group; its hosts move to the end of the ungrouped list.
    pub fn remove_group(&mut self, gid: Id) {
        if let Some(pos) = self.groups.iter().position(|g| g.id == gid) {
            let group = self.groups.remove(pos);
            self.ungrouped.extend(group.hosts);
        }
    }

    /// Place group `gid` at `index` (index into the list before the move).
    pub fn move_group(&mut self, gid: Id, index: usize) -> Result<()> {
        let old = self
            .groups
            .iter()
            .position(|g| g.id == gid)
            .context("Unknown group")?;
        let group = self.groups.remove(old);
        let index = if old < index { index - 1 } else { index };
        self.groups.insert(index.min(self.groups.len()), group);
        Ok(())
    }

    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home_dir().join(".config"));
        base.join("sshpilot-rs").join("layout.json")
    }

    pub fn load(path: &Path) -> Result<Layout> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is not a valid layout file", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Layout::default()),
            Err(e) => Err(e).with_context(|| format!("Cannot read {}", path.display())),
        }
    }

    /// Write atomically, readable by the owner only.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("Layout file has no folder")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let tmp = path.with_extension("json.tmp");
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("Cannot write {}", tmp.display()))?;
        file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(aliases: &[&str]) -> Vec<Host> {
        aliases
            .iter()
            .map(|a| Host {
                alias: (*a).into(),
                ..Default::default()
            })
            .collect()
    }

    fn sample() -> (Layout, Id) {
        let mut l = Layout::default();
        l.reconcile(&hosts(&["a", "b", "c", "d"]));
        let g = l.add_group("G").unwrap();
        l.move_host("d", Some(g), 0).unwrap();
        (l, g)
    }

    #[test]
    fn parses_hosts_and_their_settings() {
        let text = "Host *\n  User nobody\n\nHost web db # two\n  HostName 10.0.0.1\n  User=deploy\n  Port 2222\n\
                    Match host x\n  User other\nhost -bad ok\n  hostname ok.example\n  HostName ignored\n";
        let parsed = parse_ssh_config(text);
        let aliases: Vec<_> = parsed.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, ["web", "db", "ok"]);
        assert_eq!(parsed[0].target(), "deploy@10.0.0.1:2222");
        assert_eq!(parsed[1].target(), "deploy@10.0.0.1:2222");
        assert_eq!(parsed[2].target(), "ok.example");
    }

    #[test]
    fn follows_includes_like_ssh() {
        let dir = std::env::temp_dir().join(format!("sshpilot-rs-inc-{}", std::process::id()));
        let d = dir.join("config.d");
        fs::create_dir_all(&d).unwrap();
        let config = dir.join("config");
        fs::write(
            &config,
            "Host first\n  Include config.d/*.conf missing\n  User me\nInclude config\nHost last\n",
        )
        .unwrap();
        fs::write(d.join("b.conf"), "Host b\n  HostName b.example\n").unwrap();
        fs::write(d.join("a.conf"), "Host a\nInclude ../config.d/*.conf\n").unwrap();
        fs::write(d.join(".hidden.conf"), "Host hidden\n").unwrap();
        fs::write(d.join("notes.txt"), "Host txt\n").unwrap();

        let found = load_ssh_config(&config);
        let aliases: Vec<_> = found.hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, ["first", "a", "b", "last"]);
        // `User me` follows the Include, so it is still `first`'s.
        assert_eq!(found.hosts[0].target(), "me@first");
        assert_eq!(found.hosts[2].target(), "b.example");
        assert!(found.watch.contains(&config));
        assert!(found.watch.contains(&d), "watch the folder a pattern reads");
        assert!(found.watch.contains(&dir.join("missing")));
        // Plain parsing leaves Include alone.
        assert_eq!(
            parse_ssh_config(&fs::read_to_string(&config).unwrap()).len(),
            2
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wildcards() {
        for (pattern, text) in [
            ("*", "x"),
            ("*.conf", "a.conf"),
            ("a?c", "abc"),
            ("*b*", "abc"),
            ("*", ""),
        ] {
            assert!(wildcard(pattern, text), "{pattern} {text}");
        }
        for (pattern, text) in [("*.conf", "a.txt"), ("a?c", "ac"), ("abc", "abcd")] {
            assert!(!wildcard(pattern, text), "{pattern} {text}");
        }
    }

    #[test]
    fn group_ids_stay_unique_after_hand_edits() {
        let mut l: Layout = serde_json::from_str(r#"{"groups":[{"id":4,"name":"x"}]}"#).unwrap();
        assert_eq!(l.add_group("y").unwrap(), 5);
    }

    #[test]
    fn reorders_within_a_list_both_ways() {
        let (mut l, _) = sample();
        l.move_host("c", None, 0).unwrap();
        assert_eq!(l.ungrouped, ["c", "a", "b"]);
        l.move_host("c", None, 3).unwrap();
        assert_eq!(l.ungrouped, ["a", "b", "c"]);
        l.move_host("a", None, 1).unwrap();
        assert_eq!(l.ungrouped, ["a", "b", "c"]);
    }

    #[test]
    fn moves_between_groups_and_back() {
        let (mut l, g) = sample();
        l.move_host("a", Some(g), 0).unwrap();
        assert_eq!(l.group(g).unwrap().hosts, ["a", "d"]);
        l.move_host("d", None, 1).unwrap();
        assert_eq!(l.ungrouped, ["b", "d", "c"]);
    }

    #[test]
    fn follows_the_config() {
        let (mut l, g) = sample();
        assert!(!l.reconcile(&hosts(&["a", "b", "c", "d"])));
        assert!(l.reconcile(&hosts(&["d", "a", "new"])));
        assert_eq!(l.ungrouped, ["a", "new"]);
        assert_eq!(l.group(g).unwrap().hosts, ["d"]);
    }

    #[test]
    fn groups_reorder_and_delete_keeps_hosts() {
        let (mut l, g) = sample();
        let g2 = l.add_group("two").unwrap();
        l.move_group(g2, 0).unwrap();
        assert_eq!(l.groups[0].id, g2);
        l.remove_group(g);
        assert_eq!(l.ungrouped, ["a", "b", "c", "d"]);
    }

    #[test]
    fn new_host_rejects_option_injection() {
        let ok = NewHost {
            alias: "web".into(),
            hostname: "10.0.0.1".into(),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
        for bad in [
            NewHost {
                alias: "-oProxyCommand=x".into(),
                ..ok.clone()
            },
            NewHost {
                hostname: "-oProxyCommand=x".into(),
                ..ok.clone()
            },
            NewHost {
                hostname: "a\n  ProxyCommand x".into(),
                ..ok.clone()
            },
            NewHost {
                user: "-l".into(),
                ..ok.clone()
            },
            NewHost {
                port: "0".into(),
                ..ok.clone()
            },
            NewHost {
                alias: "web*".into(),
                ..ok.clone()
            },
            NewHost {
                alias: "web#x".into(),
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn appends_without_touching_existing_text() {
        let dir = std::env::temp_dir().join(format!("sshpilot-rs-cfg-{}", std::process::id()));
        let path = dir.join("config");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "Host old\n  HostName o").unwrap();
        let new = NewHost {
            alias: "web".into(),
            hostname: "w.example".into(),
            user: "me".into(),
            port: "2222".into(),
        };
        append_host(&path, &new).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("Host old\n  HostName o\n"));
        assert_eq!(parse_ssh_config(&text)[1].target(), "me@w.example:2222");
        assert!(append_host(&path, &new).is_err(), "duplicate alias");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saves_privately_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("sshpilot-rs-test-{}", std::process::id()));
        let path = dir.join("layout.json");
        let (l, _) = sample();
        l.save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(Layout::load(&path).unwrap(), l);
        fs::remove_dir_all(dir).unwrap();
    }
}
