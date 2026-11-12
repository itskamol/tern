//! Merging two diverged bundles item by item instead of asking about the whole thing.
//!
//! An item is one synced file (hosts, settings, keymap, vault). [`decide_item`] compares each
//! side's hash of the item with the hash both sides had at the last sync; only an item that
//! changed on both sides is a real conflict. `hosts.json` is merged further, per connection
//! name, with [`merge_hosts`] so two Macs that each added a host both keep both.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::Files;

/// The hash recorded for an item whose file does not exist.
pub const ABSENT: &str = "-";

/// A hash per item: SHA-256 of the file's bytes, [`ABSENT`] when the file is missing.
pub fn item_hashes(files: &Files, items: &[&str]) -> BTreeMap<String, String> {
    items
        .iter()
        .map(|name| {
            let hash = files.get(*name).map_or_else(
                || ABSENT.to_owned(),
                |bytes| {
                    Sha256::digest(bytes)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect()
                },
            );
            ((*name).to_owned(), hash)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemPlan {
    Same,
    /// Only this Mac changed it: the remote takes this Mac's copy.
    KeepLocal,
    /// Only the remote changed it: this Mac takes the remote's copy.
    TakeRemote,
    /// Both changed it (or there is no record of the last sync): needs a merge or a choice.
    Both,
}

/// The per-item version of [`crate::decide`]. `last` is the item's hash at the last sync,
/// `None` when unknown.
pub fn decide_item(local: &str, remote: &str, last: Option<&str>) -> ItemPlan {
    if local == remote {
        return ItemPlan::Same;
    }
    match last {
        None => ItemPlan::Both,
        Some(last) => match (local != last, remote != last) {
            (true, true) => ItemPlan::Both,
            (true, false) => ItemPlan::KeepLocal,
            (false, true) => ItemPlan::TakeRemote,
            (false, false) => ItemPlan::Same,
        },
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HostsMerge {
    /// The merged `hosts.json` text.
    Merged(String),
    /// Connections edited differently on both sides, or edited on one side and removed on
    /// the other: by name.
    Conflict(Vec<String>),
}

/// The newest `hosts.json` version [`merge_hosts`] will merge. A newer file may carry fields or
/// a shape this build cannot see, so merging it would drop data: it is refused instead.
pub const HOSTS_VERSION_MERGEABLE: u64 = 2;

/// The version every merge output is at least: the one `tern` writes.
const HOSTS_VERSION_FLOOR: u64 = 1;

/// One parsed `hosts.json`: its version (0 for an empty file), its connections by name, and any
/// top-level keys other than those two, which this build does not know.
struct Hosts {
    version: u64,
    connections: Vec<(String, Value)>,
    extras: Map<String, Value>,
}

fn read(text: &str) -> Result<Hosts, String> {
    if text.trim().is_empty() {
        return Ok(Hosts {
            version: 0,
            connections: Vec::new(),
            extras: Map::new(),
        });
    }
    let v: Value = serde_json::from_str(text).map_err(|e| format!("hosts.json: {e}"))?;
    let list = v
        .get("connections")
        .and_then(Value::as_array)
        .ok_or("hosts.json: no connections list")?;
    let version = match v.get("version") {
        None => 0,
        Some(n) => n
            .as_u64()
            .ok_or("hosts.json: version is not a whole number")?,
    };
    let connections = list
        .iter()
        .map(|c| {
            let name = c
                .get("name")
                .and_then(Value::as_str)
                .ok_or("hosts.json: a connection has no name")?;
            Ok((name.to_owned(), c.clone()))
        })
        .collect::<Result<_, String>>()?;
    let mut extras = v.as_object().cloned().unwrap_or_default();
    extras.remove("version");
    extras.remove("connections");
    Ok(Hosts {
        version,
        connections,
        extras,
    })
}

/// Three-way merge of `hosts.json` by connection name. `base` is the text at the last sync.
///
/// Per name: unchanged on one side takes the other side's state (an edit, an addition or a
/// removal); changed the same way on both is that; changed differently on both is a conflict.
/// Order is this Mac's, then the connections only the remote has.
///
/// The output version is the highest of the three inputs (never below 1), so a file is never
/// written back older than any copy of it. A file whose version is above
/// [`HOSTS_VERSION_MERGEABLE`] is refused, not merged: this build cannot see what it added, and
/// writing the merge would silently drop it. Top-level keys other than `version` and
/// `connections` are kept too, this Mac's value winning over the remote's.
///
/// # Errors
/// One of the three texts is not a `hosts.json`.
pub fn merge_hosts(base: &str, local: &str, remote: &str) -> Result<HostsMerge, String> {
    let (base, local, remote) = (read(base)?, read(local)?, read(remote)?);
    let version = [&base, &local, &remote]
        .iter()
        .map(|h| h.version)
        .fold(HOSTS_VERSION_FLOOR, u64::max);
    if version > HOSTS_VERSION_MERGEABLE {
        return Err(format!(
            "hosts.json: version {version} is newer than this build understands \
             ({HOSTS_VERSION_MERGEABLE}); update tern before syncing"
        ));
    }
    let mut extras = remote.extras.clone();
    extras.extend(local.extras.clone());
    let (base, local, remote) = (base.connections, local.connections, remote.connections);
    let find = |list: &[(String, Value)], name: &str| {
        list.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    };
    let mut order: Vec<&str> = Vec::new();
    for (n, _) in local.iter().chain(&remote) {
        if !order.contains(&n.as_str()) {
            order.push(n);
        }
    }
    let (mut out, mut conflicts) = (Vec::new(), Vec::new());
    for name in order {
        let (b, l, r) = (find(&base, name), find(&local, name), find(&remote, name));
        let pick = if l == r || r == b {
            l
        } else if l == b {
            r
        } else {
            conflicts.push(name.to_owned());
            continue;
        };
        out.extend(pick);
    }
    if !conflicts.is_empty() {
        return Ok(HostsMerge::Conflict(conflicts));
    }
    let mut file = Map::new();
    file.insert("version".into(), json!(version));
    file.insert("connections".into(), Value::Array(out));
    file.extend(extras);
    serde_json::to_string_pretty(&Value::Object(file))
        .map(HostsMerge::Merged)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn conn(name: &str, host: &str) -> Value {
        json!({"name": name, "host": host, "port": 22, "user": "root"})
    }

    fn file(list: &[Value]) -> String {
        serde_json::to_string_pretty(&json!({"version": 1, "connections": list})).unwrap()
    }

    fn merged(base: &[Value], local: &[Value], remote: &[Value]) -> HostsMerge {
        merge_hosts(&file(base), &file(local), &file(remote)).unwrap()
    }

    fn connections(text: &str) -> Result<Vec<(String, Value)>, String> {
        read(text).map(|h| h.connections)
    }

    fn names(m: HostsMerge) -> Vec<String> {
        let HostsMerge::Merged(text) = m else {
            return vec![format!("not a clean merge: {m:?}")];
        };
        connections(&text)
            .unwrap()
            .into_iter()
            .map(|(n, v)| format!("{n}={}", v["host"].as_str().unwrap()))
            .collect()
    }

    #[test]
    fn item_table() {
        use ItemPlan::*;
        assert_eq!(decide_item("a", "a", Some("old")), Same);
        assert_eq!(decide_item("new", "old", Some("old")), KeepLocal);
        assert_eq!(decide_item("old", "new", Some("old")), TakeRemote);
        assert_eq!(decide_item("x", "y", Some("old")), Both);
        assert_eq!(decide_item("x", "y", None), Both, "no record: ask");
        assert_eq!(decide_item("x", "x", None), Same);
        // A file created on one side only: base is absent.
        assert_eq!(decide_item("k", ABSENT, Some(ABSENT)), KeepLocal);
        assert_eq!(decide_item(ABSENT, "k", Some(ABSENT)), TakeRemote);
    }

    #[test]
    fn item_hashes_tell_files_apart() {
        let mut f = Files::new();
        f.insert("hosts.json".into(), b"{}".to_vec());
        f.insert("keymap.json".into(), b"[]".to_vec());
        let h = item_hashes(&f, &["hosts.json", "settings.json", "keymap.json"]);
        assert_eq!(h["settings.json"], ABSENT);
        assert_ne!(h["hosts.json"], h["keymap.json"]);
        assert_eq!(h["hosts.json"].len(), 64);
    }

    #[test]
    fn two_macs_add_different_hosts_keep_both() {
        let base = [conn("web", "1.1.1.1")];
        let local = [conn("web", "1.1.1.1"), conn("db", "2.2.2.2")];
        let remote = [conn("web", "1.1.1.1"), conn("cache", "3.3.3.3")];
        assert_eq!(
            names(merged(&base, &local, &remote)),
            ["web=1.1.1.1", "db=2.2.2.2", "cache=3.3.3.3"]
        );
    }

    #[test]
    fn removed_on_one_side_and_untouched_on_the_other_is_removed() {
        let base = [conn("web", "1.1.1.1"), conn("db", "2.2.2.2")];
        let removed = [conn("web", "1.1.1.1")];
        assert_eq!(names(merged(&base, &base, &removed)), ["web=1.1.1.1"]);
        // And the other way round: this Mac removed it.
        assert_eq!(names(merged(&base, &removed, &base)), ["web=1.1.1.1"]);
    }

    #[test]
    fn removed_on_one_side_and_edited_on_the_other_is_a_conflict() {
        let base = [conn("web", "1.1.1.1"), conn("db", "2.2.2.2")];
        let local = [conn("web", "1.1.1.1")];
        let remote = [conn("web", "1.1.1.1"), conn("db", "9.9.9.9")];
        assert_eq!(
            merged(&base, &local, &remote),
            HostsMerge::Conflict(vec!["db".into()])
        );
        assert_eq!(
            merged(&base, &remote, &local),
            HostsMerge::Conflict(vec!["db".into()])
        );
    }

    #[test]
    fn both_edit_different_hosts_takes_each_edit() {
        let base = [conn("web", "1.1.1.1"), conn("db", "2.2.2.2")];
        let local = [conn("web", "10.0.0.1"), conn("db", "2.2.2.2")];
        let remote = [conn("web", "1.1.1.1"), conn("db", "20.0.0.2")];
        assert_eq!(
            names(merged(&base, &local, &remote)),
            ["web=10.0.0.1", "db=20.0.0.2"]
        );
    }

    #[test]
    fn both_edit_the_same_host_differently_is_a_conflict_but_alike_is_not() {
        let base = [conn("web", "1.1.1.1"), conn("db", "2.2.2.2")];
        let local = [conn("web", "10.0.0.1"), conn("db", "5.5.5.5")];
        let remote = [conn("web", "11.0.0.1"), conn("db", "5.5.5.5")];
        assert_eq!(
            merged(&base, &local, &remote),
            HostsMerge::Conflict(vec!["web".into()])
        );
    }

    #[test]
    fn same_name_added_on_both_sides_differently_is_a_conflict() {
        let base: [Value; 0] = [];
        assert_eq!(
            merged(&base, &[conn("x", "1.1.1.1")], &[conn("x", "2.2.2.2")]),
            HostsMerge::Conflict(vec!["x".into()])
        );
        assert_eq!(
            names(merged(
                &base,
                &[conn("x", "1.1.1.1")],
                &[conn("x", "1.1.1.1")]
            )),
            ["x=1.1.1.1"]
        );
    }

    #[test]
    fn unknown_fields_survive_and_a_missing_file_is_empty() {
        let mut c = conn("web", "1.1.1.1");
        c["identityFile"] = json!("~/.ssh/id_ed25519");
        let out = merge_hosts("", &file(&[c.clone()]), "").unwrap();
        let HostsMerge::Merged(text) = &out else {
            unreachable!("{out:?}")
        };
        assert_eq!(connections(text).unwrap()[0].1, c);
    }

    // ---- properties and fault input -------------------------------------------------------

    use proptest::prelude::*;

    /// One connection's state on (base, local, remote): `None` is absent, `Some(k)` is host `h{k}`.
    type State = (Option<u8>, Option<u8>, Option<u8>);

    fn host_of(k: u8) -> String {
        format!("h{k}")
    }

    /// A side's well-formed connection list; `n{i}` names are unique by construction.
    fn side(states: &[State], pick: fn(&State) -> Option<u8>, reversed: bool) -> Vec<Value> {
        let mut list: Vec<Value> = states
            .iter()
            .enumerate()
            .filter_map(|(i, s)| pick(s).map(|k| conn(&format!("n{i}"), &host_of(k))))
            .collect();
        if reversed {
            list.reverse();
        }
        list
    }

    fn states() -> impl Strategy<Value = Vec<State>> {
        let host = prop::option::of(0u8..3);
        prop::collection::vec((host.clone(), host.clone(), host), 0..6)
    }

    fn file_v(version: u64, list: &[Value]) -> String {
        serde_json::to_string_pretty(&json!({"version": version, "connections": list})).unwrap()
    }

    fn out_conns(m: &HostsMerge) -> Vec<(String, Value)> {
        let HostsMerge::Merged(text) = m else {
            unreachable!("{m:?}")
        };
        connections(text).unwrap()
    }

    proptest! {
        #[test]
        fn a_change_on_one_side_is_never_lost(
            base_hosts in prop::collection::vec(0u8..3, 0..5),
            edited in prop::collection::vec(any::<bool>(), 5),
            added in 0usize..4,
            changed_is_local in any::<bool>(),
        ) {
            let base: Vec<Value> = base_hosts
                .iter()
                .enumerate()
                .map(|(i, k)| conn(&format!("n{i}"), &host_of(*k)))
                .collect();
            let mut changed: Vec<Value> = base
                .iter()
                .enumerate()
                .map(|(i, c)| if edited[i] { conn(&format!("n{i}"), &format!("edited{i}")) } else { c.clone() })
                .collect();
            changed.extend((0..added).map(|j| conn(&format!("a{j}"), &format!("new{j}"))));

            let m = if changed_is_local {
                merged(&base, &changed, &base)
            } else {
                merged(&base, &base, &changed)
            };
            prop_assert!(matches!(m, HostsMerge::Merged(_)), "{:?}", m);
            let out: Vec<Value> = out_conns(&m).into_iter().map(|(_, v)| v).collect();
            for c in &changed {
                prop_assert!(out.contains(c), "lost {c} from {out:?}");
            }
            prop_assert_eq!(out.len(), changed.len());
        }

        #[test]
        fn merge_is_deterministic_and_keeps_local_order_then_remote_only(s in states()) {
            let (base, local) = (side(&s, |t| t.0, false), side(&s, |t| t.1, false));
            // The remote lists its connections in the opposite order, so order is observable.
            let remote = side(&s, |t| t.2, true);
            let (a, b) = (
                merge_hosts(&file(&base), &file(&local), &file(&remote)).unwrap(),
                merge_hosts(&file(&base), &file(&local), &file(&remote)).unwrap(),
            );
            prop_assert_eq!(&a, &b);
            if let HostsMerge::Merged(_) = a {
                let name = |v: &Value| v["name"].as_str().unwrap().to_owned();
                let out: Vec<String> = out_conns(&a).into_iter().map(|(n, _)| n).collect();
                let local_names: Vec<String> = local.iter().map(name).collect();
                let remote_only: Vec<String> = remote
                    .iter()
                    .map(name)
                    .filter(|n| !local_names.contains(n))
                    .collect();
                let expected: Vec<String> = local_names
                    .iter()
                    .chain(&remote_only)
                    .filter(|n| out.contains(n))
                    .cloned()
                    .collect();
                prop_assert_eq!(out, expected);
            }
        }

        #[test]
        fn identical_change_on_both_sides_is_not_a_conflict(s in states()) {
            // Whatever base was, both sides ended up with the local state.
            let both: Vec<State> = s.iter().map(|t| (t.0, t.1, t.1)).collect();
            let (base, local) = (side(&both, |t| t.0, false), side(&both, |t| t.1, false));
            let m = merged(&base, &local, &local);
            prop_assert!(matches!(m, HostsMerge::Merged(_)), "{:?}", m);
            let out: Vec<Value> = out_conns(&m).into_iter().map(|(_, v)| v).collect();
            prop_assert_eq!(out, local);
        }

        #[test]
        fn same_connection_changed_differently_is_a_conflict(rest in states()) {
            // n0: base h0, local h1, remote h2. Noise on the other names cannot hide it.
            let mut s: Vec<State> = vec![(Some(0), Some(1), Some(2))];
            s.extend(rest);
            let m = merged(
                &side(&s, |t| t.0, false),
                &side(&s, |t| t.1, false),
                &side(&s, |t| t.2, false),
            );
            match m {
                HostsMerge::Conflict(names) => prop_assert!(names.contains(&"n0".to_owned()), "{names:?}"),
                HostsMerge::Merged(_) => prop_assert!(false, "n0 conflict went through"),
            }
        }

        #[test]
        fn a_truncated_hosts_file_is_an_error_not_a_panic(
            s in states(),
            cut in 0.0f64..1.0,
        ) {
            let text = file(&side(&s, |t| t.1, false));
            let mut at = (text.len() as f64 * cut) as usize;
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            let prefix = &text[..at];
            prop_assume!(!prefix.trim().is_empty());
            prop_assert!(merge_hosts(prefix, &text, &text).is_err());
            prop_assert!(merge_hosts(&text, prefix, &text).is_err());
            prop_assert!(merge_hosts(&text, &text, prefix).is_err());
        }
    }

    #[test]
    fn merged_output_keeps_the_input_version() {
        let local = file_v(2, &[conn("web", "1.1.1.1")]);
        let remote = file_v(2, &[conn("web", "1.1.1.1"), conn("db", "2.2.2.2")]);
        let HostsMerge::Merged(out) = merge_hosts(&local, &local, &remote).unwrap() else {
            unreachable!("remote only added a host")
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["version"], json!(2), "silently downgraded: {out}");
    }

    fn merged_version(base: &str, local: &str, remote: &str) -> Value {
        let HostsMerge::Merged(out) = merge_hosts(base, local, remote).unwrap() else {
            unreachable!("no conflict in these inputs")
        };
        serde_json::from_str::<Value>(&out).unwrap()["version"].clone()
    }

    #[test]
    fn output_version_is_the_highest_of_the_three_inputs() {
        let c = [conn("web", "1.1.1.1")];
        let (v1, v2) = (file_v(1, &c), file_v(2, &c));
        // Each position can be the one that carries the newest version.
        assert_eq!(merged_version(&v2, &v1, &v1), json!(2), "base");
        assert_eq!(merged_version(&v1, &v2, &v1), json!(2), "local");
        assert_eq!(merged_version(&v1, &v1, &v2), json!(2), "remote");
        assert_eq!(merged_version(&v1, &v1, &v1), json!(1));
    }

    #[test]
    fn a_missing_side_does_not_lower_the_version_and_the_floor_is_one() {
        let v2 = file_v(2, &[conn("web", "1.1.1.1")]);
        assert_eq!(merged_version("", &v2, ""), json!(2));
        assert_eq!(merged_version("", "", ""), json!(1));
        // A file with no version key counts as the oldest, not as an error.
        let bare = r#"{"connections": []}"#;
        assert_eq!(merged_version(bare, bare, bare), json!(1));
    }

    #[test]
    fn a_version_this_build_cannot_see_is_refused_wherever_it_sits() {
        let c = [conn("web", "1.1.1.1")];
        let (ok, new) = (file_v(1, &c), file_v(HOSTS_VERSION_MERGEABLE + 1, &c));
        for (b, l, r) in [(&new, &ok, &ok), (&ok, &new, &ok), (&ok, &ok, &new)] {
            let e = merge_hosts(b, l, r).unwrap_err();
            assert!(e.contains("newer than this build"), "{e}");
        }
        // The newest version it does understand is merged, not refused.
        let edge = file_v(HOSTS_VERSION_MERGEABLE, &c);
        assert!(merge_hosts(&edge, &edge, &edge).is_ok());
    }

    #[test]
    fn a_version_that_is_not_a_whole_number_is_an_error() {
        let good = file(&[conn("web", "1.1.1.1")]);
        for v in [r#""2""#, "1.5", "-1", "null"] {
            let bad = format!(r#"{{"version": {v}, "connections": []}}"#);
            assert!(merge_hosts(&bad, &good, &good).is_err(), "{v}");
        }
    }

    #[test]
    fn unknown_top_level_keys_survive_and_this_macs_value_wins() {
        let text = |extra: Value| {
            let mut v = json!({"version": 1, "connections": [conn("web", "1.1.1.1")]});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            v.to_string()
        };
        let base = text(json!({}));
        let local = text(json!({"theme": "mine", "shared": "local"}));
        let remote = text(json!({"groups": [1], "shared": "remote"}));
        let HostsMerge::Merged(out) = merge_hosts(&base, &local, &remote).unwrap() else {
            unreachable!("same connections")
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["theme"], json!("mine"));
        assert_eq!(v["groups"], json!([1]));
        assert_eq!(v["shared"], json!("local"));
        assert_eq!(v["version"], json!(1));
        assert_eq!(v["connections"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn malformed_hosts_text_is_an_error_in_every_position() {
        let good = file(&[conn("web", "1.1.1.1")]);
        let bad = [
            "{",
            "not json",
            "[]",
            "null",
            "{}",
            r#"{"version": 1}"#,
            r#"{"connections": {}}"#,
            r#"{"connections": "web"}"#,
            r#"{"connections": [{"host": "1.1.1.1"}]}"#,
            r#"{"connections": [{"name": 7}]}"#,
        ];
        for b in bad {
            assert!(connections(b).is_err(), "connections accepted {b:?}");
            assert!(merge_hosts(b, &good, &good).is_err(), "base {b:?}");
            assert!(merge_hosts(&good, b, &good).is_err(), "local {b:?}");
            assert!(merge_hosts(&good, &good, b).is_err(), "remote {b:?}");
        }
    }
}
