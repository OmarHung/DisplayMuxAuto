//! User notes for a shared display's inputs, shared between paired hosts.
//!
//! Some displays only report input numbers ("Input 8"), so users can note what
//! each input is. An input number names the same physical port on every host,
//! so a note set on one computer shows up on every paired computer. Each entry
//! carries its own timestamp, like `host_alias`.

use muxsu_core::{DisplayInput, InputLabel, MonitorFingerprint};

/// Longest note accepted, in characters.
pub const MAX_LABEL_CHARS: usize = 24;
/// Most entries sent in or accepted from one notice; keeps a notice well under
/// the agent's packet limit.
pub const MAX_SHARED_LABELS: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum LabelError {
    TooLong,
    ControlCharacter,
}

/// Trims `input` and checks it can be shown as an input note. An empty result
/// means "no note".
pub fn normalize_label(input: &str) -> Result<String, LabelError> {
    let label = input.trim();
    if label.chars().any(char::is_control) {
        return Err(LabelError::ControlCharacter);
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err(LabelError::TooLong);
    }
    Ok(label.to_owned())
}

fn is_entry_for(entry: &InputLabel, monitor: &MonitorFingerprint, input: DisplayInput) -> bool {
    entry.input == input && entry.monitor.matches_exactly(monitor)
}

/// The note for `input` on `monitor`, if one is set.
pub fn label_for<'a>(
    labels: &'a [InputLabel],
    monitor: &MonitorFingerprint,
    input: DisplayInput,
) -> Option<&'a str> {
    labels
        .iter()
        .find(|entry| is_entry_for(entry, monitor, input))
        .map(|entry| entry.label.as_str())
        .filter(|label| !label.is_empty())
}

/// `labels` with the note for `input` on `monitor` set to `label` (empty clears
/// it) at `now_ms`. The timestamp never moves backwards for that input, so
/// paired hosts holding the previous entry accept the change.
pub fn with_label(
    labels: &[InputLabel],
    monitor: &MonitorFingerprint,
    input: DisplayInput,
    label: String,
    now_ms: u64,
) -> Vec<InputLabel> {
    let previous = labels
        .iter()
        .find(|entry| is_entry_for(entry, monitor, input))
        .map_or(0, |entry| entry.updated_at_ms);
    let updated = InputLabel {
        monitor: monitor.clone(),
        input,
        label,
        updated_at_ms: now_ms.max(previous + 1),
    };
    labels
        .iter()
        .filter(|entry| !is_entry_for(entry, monitor, input))
        .cloned()
        .chain(std::iter::once(updated))
        .collect()
}

/// `labels` with each monitor replaced by `to_local`'s answer, so notes from a
/// paired host line up with this host's fingerprints (hosts can read a
/// display's serial number differently). Unmatched monitors are kept as sent.
pub fn with_local_monitors(
    labels: &[InputLabel],
    to_local: impl Fn(&MonitorFingerprint) -> Option<MonitorFingerprint>,
) -> Vec<InputLabel> {
    labels
        .iter()
        .map(|entry| InputLabel {
            monitor: to_local(&entry.monitor).unwrap_or_else(|| entry.monitor.clone()),
            ..entry.clone()
        })
        .collect()
}

/// `current` merged with entries from a paired host, keeping the newer entry
/// for each input. Malformed entries are skipped, and a notice with more
/// entries than `MAX_SHARED_LABELS` is rejected. Returns `None` when nothing
/// changes.
pub fn merged_labels(current: &[InputLabel], incoming: &[InputLabel]) -> Option<Vec<InputLabel>> {
    if incoming.len() > MAX_SHARED_LABELS {
        return None;
    }
    let mut merged = current.to_vec();
    let mut changed = false;
    for entry in incoming {
        if DisplayInput::new(entry.input.value()).is_err()
            || normalize_label(&entry.label).as_deref() != Ok(entry.label.as_str())
        {
            continue;
        }
        match merged
            .iter_mut()
            .find(|existing| is_entry_for(existing, &entry.monitor, entry.input))
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

/// Whether a paired host holding `theirs` (already mapped to our monitors)
/// would gain anything from our entries.
pub fn has_newer_entries(ours: &[InputLabel], theirs: &[InputLabel]) -> bool {
    merged_labels(theirs, &shareable_labels(ours)).is_some()
}

/// The newest entries that fit in one notice.
pub fn shareable_labels(labels: &[InputLabel]) -> Vec<InputLabel> {
    let mut newest = labels.to_vec();
    newest.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at_ms));
    newest.truncate(MAX_SHARED_LABELS);
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mpg(serial: Option<&str>) -> MonitorFingerprint {
        MonitorFingerprint::new("MSI", "3CF0", serial.map(str::to_owned))
    }

    fn input(value: u32) -> DisplayInput {
        DisplayInput::new(value).unwrap()
    }

    fn entry(
        monitor: &MonitorFingerprint,
        value: u32,
        label: &str,
        updated_at_ms: u64,
    ) -> InputLabel {
        InputLabel {
            monitor: monitor.clone(),
            input: input(value),
            label: label.to_owned(),
            updated_at_ms,
        }
    }

    #[test]
    fn notes_are_trimmed_and_limited_to_printable_short_text() {
        assert_eq!(normalize_label("  USB-C  "), Ok("USB-C".to_owned()));
        assert_eq!(normalize_label("   "), Ok(String::new()));
        assert_eq!(
            normalize_label(&"字".repeat(MAX_LABEL_CHARS)),
            Ok("字".repeat(MAX_LABEL_CHARS))
        );
        assert_eq!(
            normalize_label(&"字".repeat(MAX_LABEL_CHARS + 1)),
            Err(LabelError::TooLong)
        );
        assert_eq!(normalize_label("USB\tC"), Err(LabelError::ControlCharacter));
    }

    #[test]
    fn a_note_belongs_to_one_input_of_one_display() {
        let monitor = mpg(None);
        let other = MonitorFingerprint::new("ACR", "0725", Some("576726074".to_owned()));
        let labels = vec![entry(&monitor, 8, "USB-C", 10), entry(&monitor, 9, "", 20)];

        assert_eq!(label_for(&labels, &monitor, input(8)), Some("USB-C"));
        assert_eq!(label_for(&labels, &monitor, input(9)), None);
        assert_eq!(label_for(&labels, &monitor, input(7)), None);
        assert_eq!(label_for(&labels, &other, input(8)), None);
    }

    #[test]
    fn setting_a_note_replaces_the_entry_with_a_newer_timestamp() {
        let monitor = mpg(None);
        let labels = vec![
            entry(&monitor, 8, "舊備註", 500),
            entry(&monitor, 7, "DP", 20),
        ];

        let updated = with_label(&labels, &monitor, input(8), "USB-C".to_owned(), 100);

        assert_eq!(label_for(&updated, &monitor, input(8)), Some("USB-C"));
        assert_eq!(label_for(&updated, &monitor, input(7)), Some("DP"));
        let changed = updated.iter().find(|item| item.input == input(8)).unwrap();
        assert_eq!(changed.updated_at_ms, 501);
        assert_eq!(labels[0].label, "舊備註");
    }

    #[test]
    fn a_paired_host_note_lines_up_with_this_host_display() {
        let theirs = mpg(Some("PC-SERIAL"));
        let ours = mpg(None);
        let incoming = vec![entry(&theirs, 8, "USB-C", 10)];

        let local = with_local_monitors(&incoming, |monitor| {
            monitor.is_same_model(&ours).then(|| ours.clone())
        });
        let merged = merged_labels(&[], &local).unwrap();

        assert_eq!(label_for(&merged, &ours, input(8)), Some("USB-C"));
        assert_eq!(incoming[0].monitor, theirs);
    }

    #[test]
    fn merging_keeps_the_newer_entry_for_each_input() {
        let monitor = mpg(None);
        let current = vec![
            entry(&monitor, 8, "本機改的", 200),
            entry(&monitor, 7, "舊", 100),
        ];
        let incoming = vec![
            entry(&monitor, 8, "遠端改的", 150),
            entry(&monitor, 7, "DP", 300),
        ];

        let merged = merged_labels(&current, &incoming).unwrap();

        assert_eq!(label_for(&merged, &monitor, input(8)), Some("本機改的"));
        assert_eq!(label_for(&merged, &monitor, input(7)), Some("DP"));
    }

    #[test]
    fn merging_an_older_or_identical_notice_changes_nothing() {
        let monitor = mpg(None);
        let current = vec![entry(&monitor, 8, "USB-C", 200)];

        assert_eq!(
            merged_labels(&current, &[entry(&monitor, 8, "舊", 100)]),
            None
        );
        assert_eq!(merged_labels(&current, &current), None);
    }

    #[test]
    fn a_newer_clear_from_a_peer_removes_the_note() {
        let monitor = mpg(None);
        let current = vec![entry(&monitor, 8, "USB-C", 200)];

        let merged = merged_labels(&current, &[entry(&monitor, 8, "", 300)]).unwrap();

        assert_eq!(label_for(&merged, &monitor, input(8)), None);
    }

    #[test]
    fn malformed_or_oversized_notices_are_not_adopted() {
        let monitor = mpg(None);
        let invalid_input: InputLabel =
            serde_json::from_str(r#"{"monitor":{"manufacturer_id":"MSI","product_code":"3CF0","serial_number":null},"input":0,"label":"零","updatedAtMs":1}"#)
                .unwrap();

        assert_eq!(merged_labels(&[], &[invalid_input]), None);
        assert_eq!(
            merged_labels(&[], &[entry(&monitor, 8, " padded ", 1)]),
            None
        );
        assert_eq!(
            merged_labels(
                &[],
                &[entry(&monitor, 8, &"字".repeat(MAX_LABEL_CHARS + 1), 1)]
            ),
            None
        );
        let too_many: Vec<InputLabel> = (1..=MAX_SHARED_LABELS as u32 + 1)
            .map(|value| entry(&monitor, value, "備註", 1))
            .collect();
        assert_eq!(merged_labels(&[], &too_many), None);
    }

    #[test]
    fn a_peer_needs_our_notes_only_when_we_hold_something_newer() {
        let monitor = mpg(None);
        let ours = vec![
            entry(&monitor, 8, "USB-C", 200),
            entry(&monitor, 7, "DP", 50),
        ];

        assert!(has_newer_entries(&ours, &[]));
        assert!(has_newer_entries(&ours, &[entry(&monitor, 8, "舊", 100)]));
        assert!(!has_newer_entries(&ours, &ours));
        assert!(!has_newer_entries(&[], &[entry(&monitor, 8, "USB-C", 1)]));
    }

    #[test]
    fn only_the_newest_entries_are_shared() {
        let monitor = mpg(None);
        let labels: Vec<InputLabel> = (1..=MAX_SHARED_LABELS as u32 + 2)
            .map(|value| entry(&monitor, value, "備註", u64::from(value)))
            .collect();

        let shared = shareable_labels(&labels);

        assert_eq!(shared.len(), MAX_SHARED_LABELS);
        assert!(shared.iter().all(|item| item.updated_at_ms >= 3));
    }
}
