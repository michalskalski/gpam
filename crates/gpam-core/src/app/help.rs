//! Key binding reference shown by the help overlay. Frontends bind the same
//! keys, so the tables live here and every frontend renders them.

/// `(keys, description)` rows for the entitlement list.
pub const LIST_KEYS: &[(&str, &str)] = &[
    ("j / down / ^n", "move down"),
    ("k / up / ^p", "move up"),
    ("^d / ^u", "move 10"),
    ("g / G", "top / bottom"),
    ("space", "toggle select"),
    ("enter", "request marked, else current"),
    ("esc", "clear filter, then selection"),
    ("/", "focus search"),
    ("f", "cycle scope filter"),
    ("m", "show marked only"),
    ("J", "toggle raw JSON"),
    ("A", "open approval queue"),
    ("R", "refresh"),
    ("L", "show warn/error log"),
    ("?", "this help"),
    ("q / ^c", "quit"),
];

/// `(keys, description)` rows for the search field.
pub const SEARCH_KEYS: &[(&str, &str)] = &[
    ("type", "filter entries"),
    ("in:name", "only this scope"),
    ("role:name", "only entries granting this role"),
    ("up / down", "to list + move"),
    ("^n / ^p", "to list + move"),
    ("^a / ^e", "home / end"),
    ("^w", "delete word"),
    ("enter", "request match"),
    ("esc", "back to list"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_ascii_and_non_empty() {
        for (keys, desc) in LIST_KEYS.iter().chain(SEARCH_KEYS) {
            assert!(!keys.is_empty() && !desc.is_empty());
            assert!(keys.is_ascii() && desc.is_ascii());
        }
    }
}
