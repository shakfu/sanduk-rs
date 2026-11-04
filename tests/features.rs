//! Features this crate turns on that would change a crate linking it. Cargo unifies features, so
//! this test crate sees what any dependent would.

use serde_json::{Map, Value};

/// `serde_json/preserve_order` would keep insertion order; a dependent relying on sorted keys,
/// as pma's export round-trip does, would then fail.
#[test]
fn a_json_map_in_a_dependent_serialises_its_keys_sorted() {
    let mut map = Map::new();
    map.insert("b".into(), Value::from(1));
    map.insert("a".into(), Value::from(2));
    assert_eq!(Value::Object(map).to_string(), r#"{"a":2,"b":1}"#);
}

#[test]
fn a_toml_table_in_a_dependent_iterates_its_keys_sorted() {
    let table: toml::Table = toml::from_str("b = 1\na = 2\n").unwrap();
    assert_eq!(table.keys().collect::<Vec<_>>(), ["a", "b"]);
}
