//! Fan-out planning (SPEC §5.1, mailbox design §4): an envelope is deposited
//! once per **distinct relay** across all recipients — each relay indexes it
//! under the recipient keys it hosts, and receivers dedup by message id.

use std::collections::BTreeSet;

/// The distinct relays across all recipients' relay lists, in deterministic
/// (sorted) order. Generic over the relay identifier — the pure core does
/// not know transport address types.
pub fn distinct_relays<R: Ord>(per_recipient: impl IntoIterator<Item = Vec<R>>) -> Vec<R> {
    let set: BTreeSet<R> = per_recipient.into_iter().flatten().collect();
    set.into_iter().collect()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn distinct_relays__should_deposit_once_per_relay_shared_by_recipients() {
        // Given: two recipients on the same relay, one on a second
        let per_recipient = vec![vec!["relay-1"], vec!["relay-1"], vec!["relay-1", "relay-2"]];

        // When
        let relays = distinct_relays(per_recipient);

        // Then
        assert_eq!(relays, vec!["relay-1", "relay-2"]);
    }

    #[test]
    fn distinct_relays__should_be_deterministic_regardless_of_input_order() {
        let a = distinct_relays(vec![vec!["r2"], vec!["r1"]]);
        let b = distinct_relays(vec![vec!["r1"], vec!["r2"]]);
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_relays__should_be_empty_for_no_recipients() {
        assert_eq!(distinct_relays(Vec::<Vec<&str>>::new()), Vec::<&str>::new());
    }
}
