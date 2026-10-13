use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

/// The loaded project's `dispatch:` order: macro namespace → search order. Private, so
/// every writer goes through [`set_dispatch_config`] (a `get_or_init` writer kept the
/// first project's order for the life of the process, ferrion #561).
static DISPATCH_CONFIG: OnceLock<RwLock<BTreeMap<String, Vec<String>>>> = OnceLock::new();

/// The dispatch order of the project loaded last; empty before any load.
pub fn dispatch_config() -> BTreeMap<String, Vec<String>> {
    DISPATCH_CONFIG
        .get()
        .map(|lock| lock.read().unwrap_or_else(|p| p.into_inner()).clone())
        .unwrap_or_default()
}

/// Make `map` the dispatch order: the project being loaded's, replacing whatever an
/// earlier load in the same process set (ferrion #561 — a browser tab runs many
/// projects in one process, and a second one dispatched in the first's order).
pub fn set_dispatch_config(map: BTreeMap<String, Vec<String>>) {
    let lock = DISPATCH_CONFIG.get_or_init(|| RwLock::new(BTreeMap::new()));
    *lock.write().unwrap_or_else(|p| p.into_inner()) = map;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second project's dispatch order replaces the first's (ferrion #561), as a page
    /// running two projects in one process loads them.
    #[test]
    fn a_later_load_replaces_the_dispatch_order() {
        let first = BTreeMap::from([("dbt_utils".to_string(), vec!["a".to_string()])]);
        set_dispatch_config(first.clone());
        assert_eq!(dispatch_config(), first);
        set_dispatch_config(BTreeMap::new());
        assert!(dispatch_config().is_empty());
    }
}
