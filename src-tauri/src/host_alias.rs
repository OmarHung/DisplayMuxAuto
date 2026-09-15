//! Custom host names shared between paired hosts.
//!
//! Names are keyed by discovery id (see `host_order`), so a name given to a
//! host on one computer shows up for the same host on every paired computer.
//! Each entry carries its own timestamp: two hosts renaming different hosts at
//! the same time keep both names.

use displaymux_core::HostAlias;

use crate::host_order::is_valid_host_id;

/// Longest custom name accepted, in characters.
pub const MAX_ALIAS_CHARS: usize = 32;
/// Most entries sent in or accepted from one notice; keeps a notice well under
/// the agent's packet limit.
pub const MAX_SHARED_ALIASES: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum AliasError {
    TooLong,
    ControlCharacter,
}

/// Trims `input` and checks it can be shown as a host name. An empty result
/// means "use the default name".
pub fn normalize_alias(input: &str) -> Result<String, AliasError> {
    let name = input.trim();
    if name.chars().any(char::is_control) {
        return Err(AliasError::ControlCharacter);
    }
    if name.chars().count() > MAX_ALIAS_CHARS {
        return Err(AliasError::TooLong);
    }
    Ok(name.to_owned())
}

/// The custom name for `host_id`, if one is set.
pub fn alias_for<'a>(aliases: &'a [HostAlias], host_id: &str) -> Option<&'a str> {
    aliases
        .iter()
        .find(|alias| alias.host_id == host_id)
        .map(|alias| alias.name.as_str())
        .filter(|name| !name.is_empty())
}

/// `aliases` with `host_id` renamed to `name` (empty clears it) at `now_ms`.
/// The timestamp never moves backwards for that host, so paired hosts holding
/// the previous entry accept the change.
pub fn with_alias(
    aliases: &[HostAlias],
    host_id: &str,
    name: String,
    now_ms: u64,
) -> Vec<HostAlias> {
    let previous = aliases
        .iter()
        .find(|alias| alias.host_id == host_id)
        .map_or(0, |alias| alias.updated_at_ms);
    let updated = HostAlias {
        host_id: host_id.to_owned(),
        name,
        updated_at_ms: now_ms.max(previous + 1),
    };
    aliases
        .iter()
        .filter(|alias| alias.host_id != host_id)
        .cloned()
        .chain(std::iter::once(updated))
        .collect()
}

/// `current` merged with entries from a paired host, keeping the newer entry
/// for each host. Malformed entries are skipped, and a notice with more
/// entries than `MAX_SHARED_ALIASES` is rejected. Returns `None` when nothing
/// changes.
pub fn merged_aliases(current: &[HostAlias], incoming: &[HostAlias]) -> Option<Vec<HostAlias>> {
    if incoming.len() > MAX_SHARED_ALIASES {
        return None;
    }
    let mut merged = current.to_vec();
    let mut changed = false;
    for entry in incoming {
        if !is_valid_host_id(&entry.host_id)
            || normalize_alias(&entry.name).as_deref() != Ok(entry.name.as_str())
        {
            continue;
        }
        match merged
            .iter_mut()
            .find(|alias| alias.host_id == entry.host_id)
        {
            Some(existing) if existing.updated_at_ms >= entry.updated_at_ms => {}
            Some(existing) => {
                *existing = entry.clone();
                changed = true;
            }
            None => {
                merged.push(entry.clone());
                changed = true;
            }
        }
    }
    changed.then_some(merged)
}

/// The newest entries that fit in one notice.
pub fn shareable_aliases(aliases: &[HostAlias]) -> Vec<HostAlias> {
    let mut newest = aliases.to_vec();
    newest.sort_by_key(|alias| std::cmp::Reverse(alias.updated_at_ms));
    newest.truncate(MAX_SHARED_ALIASES);
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC_ID: &str = "2cf05de0c029-windows";
    const MAC_ID: &str = "aabbccddeeff-mac";

    fn alias(host_id: &str, name: &str, updated_at_ms: u64) -> HostAlias {
        HostAlias {
            host_id: host_id.to_owned(),
            name: name.to_owned(),
            updated_at_ms,
        }
    }

    #[test]
    fn names_are_trimmed_and_limited_to_printable_short_text() {
        assert_eq!(normalize_alias("  遊戲電腦  "), Ok("遊戲電腦".to_owned()));
        assert_eq!(normalize_alias("   "), Ok(String::new()));
        assert_eq!(
            normalize_alias(&"字".repeat(MAX_ALIAS_CHARS)),
            Ok("字".repeat(MAX_ALIAS_CHARS))
        );
        assert_eq!(
            normalize_alias(&"字".repeat(MAX_ALIAS_CHARS + 1)),
            Err(AliasError::TooLong)
        );
        assert_eq!(
            normalize_alias("Game\nPC"),
            Err(AliasError::ControlCharacter)
        );
    }

    #[test]
    fn cleared_names_fall_back_to_the_default() {
        let aliases = vec![alias(PC_ID, "遊戲電腦", 10), alias(MAC_ID, "", 20)];

        assert_eq!(alias_for(&aliases, PC_ID), Some("遊戲電腦"));
        assert_eq!(alias_for(&aliases, MAC_ID), None);
        assert_eq!(alias_for(&aliases, "unknown-mac"), None);
    }

    #[test]
    fn renaming_replaces_the_entry_with_a_newer_timestamp() {
        let aliases = vec![alias(PC_ID, "舊名稱", 500), alias(MAC_ID, "工作 Mac", 20)];

        let renamed = with_alias(&aliases, PC_ID, "遊戲電腦".to_owned(), 100);

        assert_eq!(alias_for(&renamed, PC_ID), Some("遊戲電腦"));
        assert_eq!(alias_for(&renamed, MAC_ID), Some("工作 Mac"));
        let entry = renamed.iter().find(|entry| entry.host_id == PC_ID).unwrap();
        assert_eq!(entry.updated_at_ms, 501);
        assert_eq!(aliases[0].name, "舊名稱");
    }

    #[test]
    fn merging_keeps_the_newer_entry_for_each_host() {
        let current = vec![alias(PC_ID, "本機改的", 200), alias(MAC_ID, "舊 Mac", 100)];
        let incoming = vec![alias(PC_ID, "遠端改的", 150), alias(MAC_ID, "新 Mac", 300)];

        let merged = merged_aliases(&current, &incoming).unwrap();

        assert_eq!(alias_for(&merged, PC_ID), Some("本機改的"));
        assert_eq!(alias_for(&merged, MAC_ID), Some("新 Mac"));
    }

    #[test]
    fn merging_an_older_or_identical_notice_changes_nothing() {
        let current = vec![alias(PC_ID, "遊戲電腦", 200)];

        assert_eq!(
            merged_aliases(&current, &[alias(PC_ID, "舊名稱", 100)]),
            None
        );
        assert_eq!(merged_aliases(&current, &current), None);
    }

    #[test]
    fn a_newer_clear_from_a_peer_removes_the_custom_name() {
        let current = vec![alias(PC_ID, "遊戲電腦", 200)];

        let merged = merged_aliases(&current, &[alias(PC_ID, "", 300)]).unwrap();

        assert_eq!(alias_for(&merged, PC_ID), None);
    }

    #[test]
    fn malformed_or_oversized_notices_are_not_adopted() {
        let current = Vec::new();

        assert_eq!(
            merged_aliases(&current, &[alias("<bad id>", "名稱", 1)]),
            None
        );
        assert_eq!(
            merged_aliases(&current, &[alias(PC_ID, " padded ", 1)]),
            None
        );
        assert_eq!(
            merged_aliases(
                &current,
                &[alias(PC_ID, &"字".repeat(MAX_ALIAS_CHARS + 1), 1)]
            ),
            None
        );
        let too_many: Vec<HostAlias> = (0..=MAX_SHARED_ALIASES)
            .map(|index| alias(&format!("host-{index}"), "名稱", 1))
            .collect();
        assert_eq!(merged_aliases(&current, &too_many), None);
    }

    #[test]
    fn only_the_newest_entries_are_shared() {
        let aliases: Vec<HostAlias> = (0..MAX_SHARED_ALIASES as u64 + 2)
            .map(|index| alias(&format!("host-{index}"), "名稱", index))
            .collect();

        let shared = shareable_aliases(&aliases);

        assert_eq!(shared.len(), MAX_SHARED_ALIASES);
        assert!(shared.iter().all(|entry| entry.updated_at_ms >= 2));
    }
}
