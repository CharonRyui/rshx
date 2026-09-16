//! The host file: which Hosts a run can touch, and how to pick them.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// One machine the command runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// The Host's identity, and the destination handed to ssh. Restricted to
    /// `[A-Za-z0-9._-]+`, and may not begin with `-`.
    pub name: String,
    /// Target overrides: written here and nowhere else.
    pub user: Option<String>,
    pub port: Option<u16>,
    pub ip: Option<IpAddr>,
}

/// The parsed host file.
#[derive(Debug)]
pub struct HostFile {
    pub path: PathBuf,
    pub hosts: Vec<Host>,
    /// Host groups by name, as declared. Selection expands them.
    groups: BTreeMap<String, Vec<String>>,
}

/// The reserved group name meaning "every Host in the file".
pub const ALL: &str = "all";

impl HostFile {
    /// The Hosts a run should touch, given the `-g` values. No groups means
    /// every Host; several groups mean their union, each Host once.
    pub fn select(&self, groups: &[String]) -> Result<Vec<&Host>> {
        if groups.is_empty() {
            return Ok(self.hosts.iter().collect());
        }

        let wanted = self.selected_names(groups)?;
        // Report in host-file order, so a run reads the same way whatever
        // order the groups were named in.
        Ok(self
            .hosts
            .iter()
            .filter(|host| wanted.contains(&host.name))
            .collect())
    }

    /// The Host names the given groups select.
    fn selected_names(&self, groups: &[String]) -> Result<BTreeSet<String>> {
        let mut selected = BTreeSet::new();
        for group in groups {
            for name in self.expand_group(group, &mut Vec::new())? {
                selected.insert(name);
            }
        }
        Ok(selected)
    }

    /// Expands one group into the names it selects, following child groups.
    fn expand_group(&self, group: &str, visiting: &mut Vec<String>) -> Result<BTreeSet<String>> {
        if group == ALL {
            return Ok(self.hosts.iter().map(|host| host.name.clone()).collect());
        }

        let Some(selectors) = self.groups.get(group) else {
            let known: Vec<&str> = self.groups.keys().map(String::as_str).collect();
            if known.is_empty() {
                bail!("no group `{group}`: the host file declares no groups");
            }
            bail!(
                "no group `{group}`; the host file declares {}",
                known.join(", ")
            );
        };

        if let Some(cycle) = visiting.iter().position(|name| name == group) {
            let mut path = visiting[cycle..].to_vec();
            path.push(group.to_string());
            bail!("groups form a cycle: {}", path.join(" -> "));
        }
        visiting.push(group.to_string());

        let mut names = BTreeSet::new();
        for selector in selectors {
            // A child group is a selector that names a group rather than a Host.
            if self.groups.contains_key(selector) {
                names.extend(self.expand_group(selector, visiting)?);
                continue;
            }

            let matched = self
                .match_selector(selector)
                .with_context(|| format!("group `{group}` selects `{selector}`"))?;
            if matched.is_empty() {
                bail!(
                    "group `{group}` selects `{selector}`, which matches no Host \
                     in the host file; a group that silently selects nothing is a typo"
                );
            }
            names.extend(matched);
        }

        visiting.pop();
        Ok(names)
    }

    /// The declared Host names a selector matches: a literal name, or every
    /// Host a host pattern expands to.
    fn match_selector(&self, selector: &str) -> Result<BTreeSet<String>> {
        if !selector.contains('[') {
            let known = self.hosts.iter().any(|host| host.name == selector);
            return Ok(if known {
                BTreeSet::from([selector.to_string()])
            } else {
                BTreeSet::new()
            });
        }

        let expanded = expand(selector).map_err(|msg| anyhow::anyhow!("{msg}"))?;
        Ok(expanded
            .into_iter()
            .filter(|name| self.hosts.iter().any(|host| &host.name == name))
            .collect())
    }
}

/// Where the host file is, given the command line. An explicit `-H` is used as
/// written, so a typo is reported rather than silently falling back.
pub fn resolve_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }

    let local = PathBuf::from("rshx.toml");
    if local.exists() {
        return Ok(local);
    }

    if let Some(path) = config_dir().map(|dir| dir.join("rshx").join("hosts.toml"))
        && path.exists()
    {
        return Ok(path);
    }

    bail!("no host file: pass --host-file FILE, or create ./rshx.toml")
}

fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".config"))
}

/// Reads and validates the host file.
pub fn load(path: &Path) -> Result<HostFile> {
    parse(path).with_context(|| path.display().to_string())
}

fn parse(path: &Path) -> Result<HostFile> {
    let text = fs::read_to_string(path).map_err(|err| anyhow::anyhow!("{err}"))?;

    let raw: RawFile = toml::from_str(&text).map_err(|err| anyhow::anyhow!("{err}"))?;

    let mut hosts = Vec::new();
    let mut declared: BTreeMap<String, usize> = BTreeMap::new();

    for (index, entry) in raw.hosts.iter().enumerate() {
        let number = index + 1;
        let entry_label = format!("entry {number} (`{}`)", entry.name);
        let pattern = entry.name.contains('[');

        if let Some(user) = &entry.user {
            validate_user(user).map_err(|msg| anyhow::anyhow!("{entry_label}: {msg}"))?;
        }
        // An address is a property of one machine, so it cannot be shared by
        // every Host a pattern expands to.
        if entry.ip.is_some() && pattern {
            bail!(
                "{entry_label}: `ip` cannot be set on a host pattern; \
                 an address belongs to one Host, so declare them separately"
            );
        }

        for name in expand(&entry.name).map_err(|msg| anyhow::anyhow!("{entry_label}: {msg}"))? {
            validate_name(&name).map_err(|msg| anyhow::anyhow!("{entry_label}: {msg}"))?;

            if let Some(&first) = declared.get(&name) {
                bail!(
                    "entries {} (`{}`) and {number} (`{}`) both declare `{name}`",
                    first + 1,
                    raw.hosts[first].name,
                    entry.name,
                );
            }

            declared.insert(name.clone(), index);
            hosts.push(Host {
                name,
                user: entry.user.clone(),
                port: entry.port,
                ip: entry.ip,
            });
        }
    }

    Ok(HostFile {
        path: path.to_path_buf(),
        hosts,
        groups: parse_groups(&raw.groups)?,
    })
}

/// Reads the `[groups]` table, rejecting the reserved name.
fn parse_groups(raw: &BTreeMap<String, Vec<String>>) -> Result<BTreeMap<String, Vec<String>>> {
    if raw.contains_key(ALL) {
        bail!("`{ALL}` is reserved and cannot be declared as a group");
    }
    Ok(raw.clone())
}

/// A Host's name is an ssh argv element, so it is validated against a
/// whitelist rather than escaped.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a name may not be empty".into());
    }
    if name.starts_with('-') {
        return Err(format!("name `{name}` may not begin with `-`"));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "name `{name}` contains `{bad}`; a name may contain only letters, digits, `.`, `_` and `-`"
        ));
    }
    Ok(())
}

/// A user name is an ssh argv element too, since it is passed as
/// `-o User=…`. It gets the same whitelist treatment as a Host name, so a
/// host file cannot inject anything into ssh's argument list.
fn validate_user(user: &str) -> Result<(), String> {
    if user.is_empty() {
        return Err("`user` may not be empty".into());
    }
    if let Some(bad) = user
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "user `{user}` contains `{bad}`; a user may contain only letters, digits, `.`, `_` and `-`"
        ));
    }
    Ok(())
}

/// Expands a host entry's `name` into the Hosts it declares.
///
/// A name has at most one bracketed range: literal text, the range, literal
/// text. Anything the syntax cannot express is an error rather than a guess,
/// because a pattern that silently expands to the wrong Hosts would run a
/// command somewhere nobody asked for.
fn expand(name: &str) -> Result<Vec<String>, String> {
    let Some(open) = name.find('[') else {
        if name.contains(']') {
            return Err(format!("`{name}` has a `]` with no `[`"));
        }
        return Ok(vec![name.to_string()]);
    };

    let prefix = &name[..open];
    if prefix.contains(']') {
        return Err(format!("`{name}` has a `]` before its `[`"));
    }

    let rest = &name[open + 1..];
    let Some(close) = rest.find(']') else {
        return Err(format!("`{name}` opens a `[` that is never closed"));
    };
    let body = &rest[..close];
    let suffix = &rest[close + 1..];
    if suffix.contains('[') || suffix.contains(']') {
        return Err(format!(
            "`{name}` has more than one bracketed range; a name may have only one"
        ));
    }

    if body.is_empty() {
        return Err(format!("`{name}` has an empty range"));
    }

    // Parse every range and count the expansion before allocating anything: a
    // range written with too many digits is a typo, and building the names
    // first would exhaust memory long before the cap could reject it.
    let mut ranges = Vec::new();
    let mut total: usize = 0;
    for item in body.split(',') {
        let (low, high) = match item.split_once('-') {
            Some((low, high)) => {
                if high.contains('-') {
                    return Err(format!(
                        "`{name}`: `{item}` is not a range; strides are not supported"
                    ));
                }
                if low.is_empty() || high.is_empty() {
                    return Err(format!("`{name}`: `{item}` is an incomplete range"));
                }
                (low, high)
            }
            None => (item, item),
        };

        let start = parse_bound(name, low)?;
        let end = parse_bound(name, high)?;
        if start > end {
            return Err(format!(
                "`{name}`: range `{item}` counts down; ranges must ascend"
            ));
        }

        total = total.saturating_add((end - start) as usize + 1);
        if total > MAX_EXPANSION {
            return Err(format!(
                "`{name}` expands to more than {MAX_EXPANSION} hosts; check the range"
            ));
        }
        // The width of the lower bound as written decides the zero padding.
        ranges.push((start, end, low.len()));
    }

    let mut names = Vec::with_capacity(total);
    for (start, end, width) in ranges {
        for number in start..=end {
            names.push(format!("{prefix}{number:0width$}{suffix}"));
        }
    }

    Ok(names)
}

/// The most Hosts one host entry may expand to. A range written with too many
/// digits is a typo, and expanding it would exhaust memory before any host ran.
const MAX_EXPANSION: usize = 65_536;

fn parse_bound(name: &str, bound: &str) -> Result<u32, String> {
    bound
        .parse::<u32>()
        .map_err(|_| format!("`{name}`: `{bound}` is not a number"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    hosts: Vec<RawEntry>,
    /// Host groups: each name maps to the selectors that pick its Hosts. A
    /// selector is a Host name, a host pattern, or another group's name.
    #[serde(default)]
    groups: BTreeMap<String, Vec<String>>,
}

/// The expansion cap and its boundary, tested directly: going through the
/// binary would mean spawning one ssh process per expanded Host.
#[cfg(test)]
mod tests {
    use super::{MAX_EXPANSION, expand};

    #[test]
    fn a_range_at_the_cap_expands_and_one_over_it_is_rejected() {
        let at_the_cap = expand(&format!("n[0-{}]", MAX_EXPANSION - 1)).unwrap();
        assert_eq!(at_the_cap.len(), MAX_EXPANSION);

        let over = expand(&format!("n[0-{MAX_EXPANSION}]")).unwrap_err();
        assert!(over.contains("expands to more than"), "{over}");
    }

    #[test]
    fn the_cap_counts_every_item_in_the_range_list() {
        // Each item is under the cap on its own; together they are over it.
        let half = MAX_EXPANSION / 2 + 1;
        let name = format!("n[0-{},0-{}]", half - 1, half - 1);
        let err = expand(&name).unwrap_err();
        assert!(err.contains("expands to more than"), "{err}");
    }

    #[test]
    fn an_oversized_range_does_not_depend_on_its_size() {
        // The check happens before allocation, so a billion-wide range costs
        // the same as a two-wide one.
        for name in [
            "n[1-999999999]",
            "n[0-4294967295]",
            "n[0-65536]",
            "n[0-65536,0-1]",
        ] {
            let start = std::time::Instant::now();
            let err = expand(name).unwrap_err();
            assert!(err.contains("expands to more than"), "{name}: {err}");
            assert!(
                start.elapsed() < std::time::Duration::from_millis(50),
                "{name} took {:?}",
                start.elapsed()
            );
        }
    }

    #[test]
    fn bounds_are_parsed_as_numbers_not_strings() {
        // Lexically "9" sorts above "10", so a string comparison would call
        // this range descending.
        assert_eq!(expand("n[9-10]").unwrap(), ["n9", "n10"]);
        assert_eq!(expand("n[09-10]").unwrap(), ["n09", "n10"]);
    }

    #[test]
    fn leading_zeros_are_preserved_only_up_to_the_lower_bound_width() {
        assert_eq!(expand("n[000-2]").unwrap(), ["n000", "n001", "n002"]);
        assert_eq!(expand("n[8-10]").unwrap(), ["n8", "n9", "n10"]);
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    name: String,
    user: Option<String>,
    port: Option<u16>,
    ip: Option<IpAddr>,
}
