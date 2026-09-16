//! User-declared equivalences between the EDID identities of one display,
//! shared between paired hosts.
//!
//! A display is normally identified by its full EDID fingerprint, and that is
//! what makes switching safe. Some displays break the assumption that the
//! fingerprint is stable: the MSI MPG 274U publishes `MSI:3CF0` at 3840x2160
//! and `MSI:7CF0` at 1920x1080, carries no serial number in either EDID, and
//! reports the same two product codes through macOS's own display record. A
//! mode switch therefore reads as a different display on every host at once,
//! so the equivalence cannot be derived — only the user can assert it.
//!
//! A claim maps one `alias` identity onto one `primary` identity. Each entry
//! carries its own timestamp, like `input_label`, so paired hosts merge entry
//! by entry. Claims never widen what may be *switched*: they decide which
//! identities count as the same shared display, while every write still
//! demands an exact fingerprint match against a display present right now.

use displaymux_core::{MonitorFingerprint, MonitorIdentityLink};

/// Most entries sent in or accepted from one notice; keeps a notice well under
/// the agent's packet limit.
pub const MAX_SHARED_LINKS: usize = 32;

/// Longest alias chain followed before giving up, so a malformed or hostile
/// notice cannot spin `primary_for` on a cycle.
const MAX_CHAIN_DEPTH: usize = 8;

fn is_entry_for(entry: &MonitorIdentityLink, alias: &MonitorFingerprint) -> bool {
    entry.alias.matches_exactly(alias)
}

fn linked_primary<'a>(
    links: &'a [MonitorIdentityLink],
    alias: &MonitorFingerprint,
) -> Option<&'a MonitorFingerprint> {
    links
        .iter()
        .find(|entry| is_entry_for(entry, alias))
        .and_then(|entry| entry.primary.as_ref())
}

/// The identity `fingerprint` should be treated as, following the user's
/// claims. An identity with no claim is its own primary, and a chain that
/// loops or runs too deep resolves to the last identity reached rather than
/// spinning.
pub fn primary_for<'a>(
    links: &'a [MonitorIdentityLink],
    fingerprint: &'a MonitorFingerprint,
) -> &'a MonitorFingerprint {
    let mut current = fingerprint;
    for _ in 0..MAX_CHAIN_DEPTH {
        match linked_primary(links, current) {
            // A claim pointing at itself is not a step; stop rather than loop.
            Some(next) if !next.matches_exactly(current) => current = next,
            _ => return current,
        }
    }
    current
}

/// Every identity that resolves to `primary`, `primary` itself first. Used to
/// decide whether a display present right now is the selected shared display.
pub fn identities_for(
    links: &[MonitorIdentityLink],
    primary: &MonitorFingerprint,
) -> Vec<MonitorFingerprint> {
    let mut identities = vec![primary.clone()];
    for entry in links {
        if entry.primary.is_none() || entry.alias.matches_exactly(primary) {
            continue;
        }
        if primary_for(links, &entry.alias).matches_exactly(primary) {
            identities.push(entry.alias.clone());
        }
    }
    identities
}

/// Whether two identities name one physical display. Being the same display is
/// an equivalence, so both sides are resolved: a stored selection can itself be
/// an alias, and comparing only the observed side would never match it.
pub fn is_same_display(
    links: &[MonitorIdentityLink],
    left: &MonitorFingerprint,
    right: &MonitorFingerprint,
) -> bool {
    primary_for(links, left).matches_exactly(primary_for(links, right))
}

/// `links` with `alias` claimed as `primary` (`None` withdraws the claim) at
/// `now_ms`. The timestamp never moves backwards for that alias, so paired
/// hosts holding the previous entry accept the change.
pub fn with_link(
    links: &[MonitorIdentityLink],
    alias: &MonitorFingerprint,
    primary: Option<&MonitorFingerprint>,
    now_ms: u64,
) -> Vec<MonitorIdentityLink> {
    let previous = links
        .iter()
        .find(|entry| is_entry_for(entry, alias))
        .map_or(0, |entry| entry.updated_at_ms);
    let updated = MonitorIdentityLink {
        alias: alias.clone(),
        primary: primary.cloned(),
        updated_at_ms: now_ms.max(previous + 1),
    };
    links
        .iter()
        .filter(|entry| !is_entry_for(entry, alias))
        .cloned()
        .chain(std::iter::once(updated))
        .collect()
}

/// `current` merged with claims from a paired host, keeping the newer entry
/// for each alias. A claim an identity makes about itself is skipped, and a
/// notice with more entries than `MAX_SHARED_LINKS` is rejected. Returns
/// `None` when nothing changes.
pub fn merged_links(
    current: &[MonitorIdentityLink],
    incoming: &[MonitorIdentityLink],
) -> Option<Vec<MonitorIdentityLink>> {
    if incoming.len() > MAX_SHARED_LINKS {
        return None;
    }
    let mut merged = current.to_vec();
    let mut changed = false;
    for entry in incoming {
        if entry
            .primary
            .as_ref()
            .is_some_and(|primary| primary.matches_exactly(&entry.alias))
        {
            continue;
        }
        match merged
            .iter_mut()
            .find(|existing| is_entry_for(existing, &entry.alias))
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

/// Whether a paired host holding `theirs` is missing anything in `ours`, so a
/// notice is only sent when it would change something.
pub fn needs_push(ours: &[MonitorIdentityLink], theirs: &[MonitorIdentityLink]) -> bool {
    ours.iter().any(|entry| {
        !theirs.iter().any(|other| {
            is_entry_for(other, &entry.alias) && other.updated_at_ms >= entry.updated_at_ms
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(product: &str) -> MonitorFingerprint {
        MonitorFingerprint::new("MSI", product, None::<String>)
    }

    fn link(alias: &str, primary: Option<&str>, updated_at_ms: u64) -> MonitorIdentityLink {
        MonitorIdentityLink {
            alias: fingerprint(alias),
            primary: primary.map(fingerprint),
            updated_at_ms,
        }
    }

    #[test]
    fn an_identity_without_a_claim_is_its_own_primary() {
        let alone = fingerprint("3CF0");

        assert_eq!(primary_for(&[], &alone), &alone);
    }

    #[test]
    fn a_claimed_identity_resolves_to_the_display_it_was_merged_into() {
        let links = [link("7CF0", Some("3CF0"), 10)];

        assert_eq!(
            primary_for(&links, &fingerprint("7CF0")),
            &fingerprint("3CF0")
        );
        assert!(is_same_display(
            &links,
            &fingerprint("3CF0"),
            &fingerprint("7CF0")
        ));
    }

    #[test]
    fn a_withdrawn_claim_stops_resolving() {
        let links = with_link(
            &[link("7CF0", Some("3CF0"), 10)],
            &fingerprint("7CF0"),
            None,
            20,
        );

        assert_eq!(
            primary_for(&links, &fingerprint("7CF0")),
            &fingerprint("7CF0")
        );
        assert!(!is_same_display(
            &links,
            &fingerprint("3CF0"),
            &fingerprint("7CF0")
        ));
    }

    #[test]
    fn a_stored_selection_that_is_itself_an_alias_still_matches_its_display() {
        // A selection stored under the alias must still recognise itself and
        // the display it points at; otherwise nothing matches it and every
        // "add to shared" makes another copy.
        let links = [link("7CF0", Some("3CF0"), 10)];

        assert!(is_same_display(
            &links,
            &fingerprint("7CF0"),
            &fingerprint("7CF0")
        ));
        assert!(is_same_display(
            &links,
            &fingerprint("7CF0"),
            &fingerprint("3CF0")
        ));
    }

    #[test]
    fn two_aliases_of_one_display_name_the_same_display() {
        let links = [
            link("7CF0", Some("3CF0"), 10),
            link("5CF0", Some("3CF0"), 10),
        ];

        assert!(is_same_display(
            &links,
            &fingerprint("7CF0"),
            &fingerprint("5CF0")
        ));
    }

    #[test]
    fn an_unrelated_identity_is_never_treated_as_the_shared_display() {
        let links = [link("7CF0", Some("3CF0"), 10)];

        assert!(!is_same_display(
            &links,
            &fingerprint("3CF0"),
            &MonitorFingerprint::new("ACR", "0725", Some("576726074".to_owned()))
        ));
    }

    #[test]
    fn a_serial_number_still_separates_two_displays_of_the_same_model() {
        let ours = MonitorFingerprint::new("DEL", "A1B2", Some("first".to_owned()));
        let theirs = MonitorFingerprint::new("DEL", "A1B2", Some("second".to_owned()));

        assert!(!is_same_display(&[], &ours, &theirs));
    }

    #[test]
    fn a_chain_of_claims_resolves_to_the_end_of_the_chain() {
        let links = [
            link("7CF0", Some("5CF0"), 10),
            link("5CF0", Some("3CF0"), 10),
        ];

        assert_eq!(
            primary_for(&links, &fingerprint("7CF0")),
            &fingerprint("3CF0")
        );
    }

    #[test]
    fn a_looping_claim_resolves_instead_of_spinning() {
        let links = [
            link("7CF0", Some("3CF0"), 10),
            link("3CF0", Some("7CF0"), 10),
        ];

        // Either end is an acceptable answer; not hanging is the point.
        let start = fingerprint("7CF0");
        let resolved = primary_for(&links, &start);
        assert!(resolved == &fingerprint("3CF0") || resolved == &fingerprint("7CF0"));
    }

    #[test]
    fn every_alias_of_a_display_is_listed_with_the_primary_first() {
        let links = [
            link("7CF0", Some("3CF0"), 10),
            link("5CF0", Some("3CF0"), 10),
        ];

        assert_eq!(
            identities_for(&links, &fingerprint("3CF0")),
            vec![
                fingerprint("3CF0"),
                fingerprint("7CF0"),
                fingerprint("5CF0")
            ]
        );
    }

    #[test]
    fn a_display_without_claims_lists_only_itself() {
        assert_eq!(
            identities_for(&[], &fingerprint("3CF0")),
            vec![fingerprint("3CF0")]
        );
    }

    #[test]
    fn setting_a_claim_again_moves_its_timestamp_forward() {
        let first = with_link(&[], &fingerprint("7CF0"), Some(&fingerprint("3CF0")), 10);
        let second = with_link(&first, &fingerprint("7CF0"), None, 5);

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].updated_at_ms, 11);
    }

    #[test]
    fn a_paired_host_claim_is_adopted_and_an_older_one_is_ignored() {
        let ours = [link("7CF0", Some("3CF0"), 20)];

        assert_eq!(merged_links(&ours, &[link("7CF0", None, 10)]), None);
        assert_eq!(
            merged_links(&ours, &[link("7CF0", None, 30)]),
            Some(vec![link("7CF0", None, 30)])
        );
    }

    #[test]
    fn a_claim_an_identity_makes_about_itself_is_skipped() {
        assert_eq!(merged_links(&[], &[link("3CF0", Some("3CF0"), 10)]), None);
    }

    #[test]
    fn an_oversized_notice_is_rejected_whole() {
        let incoming = (0..=MAX_SHARED_LINKS)
            .map(|index| link(&format!("{index:04X}"), Some("3CF0"), 10))
            .collect::<Vec<_>>();

        assert_eq!(merged_links(&[], &incoming), None);
    }

    #[test]
    fn a_notice_is_only_pushed_when_the_paired_host_is_behind() {
        let ours = [link("7CF0", Some("3CF0"), 20)];

        assert!(needs_push(&ours, &[]));
        assert!(needs_push(&ours, &[link("7CF0", Some("3CF0"), 10)]));
        assert!(!needs_push(&ours, &[link("7CF0", Some("3CF0"), 20)]));
    }
}
