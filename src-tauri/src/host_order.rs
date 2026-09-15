//! Host card ordering shared between paired hosts.
//!
//! The dashboard names hosts by route id: `"local"` for this computer and a
//! peer id for each paired host. Paired hosts cannot use `"local"` to refer to
//! each other, so the saved order uses each host's discovery id instead
//! (`LocalHostIdentity::id`), which is the same value every peer stores.

use std::collections::HashSet;

/// Route id the dashboard uses for this computer.
pub const LOCAL_ROUTE_ID: &str = "local";
/// Upper bound on hosts accepted in an order received from a paired host.
const MAX_SHARED_HOSTS: usize = 64;
/// Upper bound on a single host id received from a paired host.
const MAX_HOST_ID_LEN: usize = 128;

/// Route ids for this computer and `peer_ids`, sorted by `host_order`. Hosts
/// missing from the order keep their original relative position after the
/// ordered ones, so a newly paired host appears last.
pub fn ordered_route_ids(
    host_order: &[String],
    local_host_id: &str,
    peer_ids: &[&str],
) -> Vec<String> {
    let mut routes: Vec<(usize, usize, &str)> = std::iter::once(LOCAL_ROUTE_ID)
        .chain(peer_ids.iter().copied())
        .enumerate()
        .map(|(original, route)| {
            let host_id = if route == LOCAL_ROUTE_ID {
                local_host_id
            } else {
                route
            };
            let rank = host_order
                .iter()
                .position(|ordered| ordered == host_id)
                .unwrap_or(usize::MAX);
            (rank, original, route)
        })
        .collect();
    routes.sort_unstable_by_key(|(rank, original, _)| (*rank, *original));
    routes
        .into_iter()
        .map(|(_, _, route)| route.to_owned())
        .collect()
}

/// Converts a dashboard route order into a host order to save and share.
/// `route_ids` must list this computer and every paired host exactly once.
/// Hosts in `previous` that this computer does not know (paired only with
/// another host) keep their relative order after the known ones. Returns
/// `None` when `route_ids` is not such a list.
pub fn host_order_from_route_ids(
    route_ids: &[String],
    local_host_id: &str,
    peer_ids: &[&str],
    previous: &[String],
) -> Option<Vec<String>> {
    let expected: HashSet<&str> = std::iter::once(LOCAL_ROUTE_ID)
        .chain(peer_ids.iter().copied())
        .collect();
    let supplied: HashSet<&str> = route_ids.iter().map(String::as_str).collect();
    if route_ids.len() != expected.len() || supplied != expected {
        return None;
    }
    let known: Vec<String> = route_ids
        .iter()
        .map(|route| {
            if route == LOCAL_ROUTE_ID {
                local_host_id.to_owned()
            } else {
                route.clone()
            }
        })
        .collect();
    let unknown = previous
        .iter()
        .filter(|host| !known.contains(host))
        .cloned();
    let order: Vec<String> = known.iter().cloned().chain(unknown).collect();
    Some(order)
}

/// Whether a host order received from a paired host is well formed: bounded,
/// free of duplicates, and made of discovery-id characters only.
pub fn is_valid_shared_host_order(order: &[String]) -> bool {
    let mut seen = HashSet::new();
    order.len() <= MAX_SHARED_HOSTS
        && order
            .iter()
            .all(|host| is_valid_host_id(host) && seen.insert(host.as_str()))
}

/// Whether `host` looks like a discovery id: bounded and limited to the
/// characters `LocalHostIdentity` produces.
pub fn is_valid_host_id(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= MAX_HOST_ID_LEN
        && host
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_ID: &str = "aabbccddeeff-mac";
    const PC_ID: &str = "2cf05de0c029-windows";
    const NAS_ID: &str = "112233445566-windows";

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn routes_follow_the_saved_host_order_with_this_computer_mapped_to_local() {
        let order = owned(&[PC_ID, MAC_ID]);

        assert_eq!(
            ordered_route_ids(&order, MAC_ID, &[PC_ID]),
            owned(&[PC_ID, "local"])
        );
    }

    #[test]
    fn the_same_saved_order_reads_correctly_on_the_other_host() {
        let order = owned(&[PC_ID, MAC_ID]);

        assert_eq!(
            ordered_route_ids(&order, PC_ID, &[MAC_ID]),
            owned(&["local", MAC_ID])
        );
    }

    #[test]
    fn hosts_missing_from_the_order_keep_their_default_position_at_the_end() {
        let order = owned(&[NAS_ID]);

        assert_eq!(
            ordered_route_ids(&order, MAC_ID, &[PC_ID, NAS_ID]),
            owned(&[NAS_ID, "local", PC_ID])
        );
        assert_eq!(
            ordered_route_ids(&[], MAC_ID, &[PC_ID]),
            owned(&["local", PC_ID])
        );
    }

    #[test]
    fn route_order_becomes_a_shareable_host_order() {
        let order = host_order_from_route_ids(&owned(&[PC_ID, "local"]), MAC_ID, &[PC_ID], &[]);

        assert_eq!(order, Some(owned(&[PC_ID, MAC_ID])));
    }

    #[test]
    fn hosts_only_another_computer_knows_stay_after_the_reordered_ones() {
        let previous = owned(&[NAS_ID, MAC_ID, PC_ID]);

        let order =
            host_order_from_route_ids(&owned(&[PC_ID, "local"]), MAC_ID, &[PC_ID], &previous);

        assert_eq!(order, Some(owned(&[PC_ID, MAC_ID, NAS_ID])));
    }

    #[test]
    fn route_order_must_list_every_known_host_exactly_once() {
        let peers = [PC_ID];

        assert_eq!(
            host_order_from_route_ids(&owned(&["local"]), MAC_ID, &peers, &[]),
            None
        );
        assert_eq!(
            host_order_from_route_ids(&owned(&["local", "local"]), MAC_ID, &peers, &[]),
            None
        );
        assert_eq!(
            host_order_from_route_ids(&owned(&["local", NAS_ID]), MAC_ID, &peers, &[]),
            None
        );
        assert_eq!(
            host_order_from_route_ids(&owned(&["local", PC_ID, PC_ID]), MAC_ID, &peers, &[]),
            None
        );
    }

    #[test]
    fn shared_host_order_rejects_malformed_input_from_the_network() {
        assert!(is_valid_shared_host_order(&owned(&[PC_ID, MAC_ID])));
        assert!(is_valid_shared_host_order(&[]));
        assert!(!is_valid_shared_host_order(&owned(&[PC_ID, PC_ID])));
        assert!(!is_valid_shared_host_order(&owned(&[""])));
        assert!(!is_valid_shared_host_order(&owned(&["<script>"])));
        assert!(!is_valid_shared_host_order(&[
            "a".repeat(MAX_HOST_ID_LEN + 1)
        ]));
        let too_many: Vec<String> = (0..=MAX_SHARED_HOSTS)
            .map(|index| format!("host-{index}"))
            .collect();
        assert!(!is_valid_shared_host_order(&too_many));
    }
}
