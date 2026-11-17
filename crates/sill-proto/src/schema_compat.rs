//! The sfp/1 freeze guard: is a protocol document (`schema::document`) a compatible extension
//! of the recorded one (`docs/sfp-1.schema.json`)? docs/protocol.md "Extension rule after the
//! freeze": additions only — new methods (behind a capability), optional fields, enum variants,
//! error codes and details. Removing, renaming, retyping or newly requiring anything is `sfp/2`.
//!
//! Types are compared through the `$ref`s that methods, errors and streams reach, never by
//! `$defs` name: schemars numbers clashing names (`List2`), so an unrelated addition may rename
//! a definition without changing a byte on the wire.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

/// Documentation only; changing it never changes the wire.
const IGNORED: &[&str] = &["description", "title", "default", "examples", "$comment"];

/// Every way `new` breaks a client or daemon written against `old`; empty when compatible.
pub fn breaking_changes(old: &Value, new: &Value) -> Vec<String> {
    let mut c = Cmp { old, new, memo: HashMap::new() };
    let mut out = Vec::new();
    if old["protocol"] != new["protocol"] {
        out.push(format!("protocol identity changed: {} → {}", old["protocol"], new["protocol"]));
    }
    let methods = |doc: &'_ Value| -> Vec<(String, Value)> {
        doc["methods"].as_array().into_iter().flatten().map(|m| (m["name"].as_str().unwrap_or_default().to_owned(), m.clone())).collect()
    };
    let (old_methods, new_methods): (HashMap<_, _>, HashMap<_, _>) = (methods(old).into_iter().collect(), methods(new).into_iter().collect());
    let mut names: Vec<&String> = old_methods.keys().collect();
    names.sort();
    for name in names {
        let o = &old_methods[name];
        let Some(n) = new_methods.get(name) else {
            out.push(format!("{name}: method removed"));
            continue;
        };
        if o["kind"] != n["kind"] {
            out.push(format!("{name}: kind changed {} → {}", o["kind"], n["kind"]));
        }
        if o["capabilities"] != n["capabilities"] {
            out.push(format!("{name}: capability requirement changed {} → {}", o["capabilities"], n["capabilities"]));
        }
        for part in ["params", "result"] {
            match (o[part].is_null(), n[part].is_null()) {
                (true, true) => {}
                (false, false) => {
                    c.node(&o[part], &n[part], &format!("{name} {part}"), &mut out);
                }
                _ => out.push(format!("{name} {part}: presence changed")),
            }
        }
    }
    let mut added: Vec<&String> = new_methods.keys().filter(|n| !old_methods.contains_key(*n)).collect();
    added.sort();
    for name in added {
        if new_methods[name]["capabilities"].as_object().is_none_or(|c| c.is_empty()) {
            out.push(format!("{name}: a method added after the freeze must require a capability"));
        }
    }
    let mut seen = BTreeSet::new();
    let mut out: Vec<String> = out.into_iter().filter(|line| seen.insert(line.clone())).collect();
    let offered = |doc: &Value| -> HashMap<String, u64> {
        doc["capabilities"]["offered"].as_array().into_iter().flatten().filter_map(|c| Some((c["name"].as_str()?.to_owned(), c["version"].as_u64()?))).collect()
    };
    let new_offered = offered(new);
    for (name, version) in offered(old) {
        match new_offered.get(&name) {
            None => out.push(format!("capability {name} removed")),
            Some(v) if *v < version => out.push(format!("capability {name} version lowered {version} → {v}")),
            _ => {}
        }
    }
    let codes =
        |doc: &Value| -> BTreeSet<String> { doc["errors"]["codes"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(str::to_owned)).collect() };
    for code in codes(old).difference(&codes(new)) {
        out.push(format!("error code {code} removed"));
    }
    if old["errors"]["domain_numeric_code"] != new["errors"]["domain_numeric_code"] {
        out.push("domain error numeric code changed".into());
    }
    c.node(&old["errors"]["shape"], &new["errors"]["shape"], "error shape", &mut out);
    c.node(&old["errors"]["domain_data"], &new["errors"]["domain_data"], "error data", &mut out);
    for (code, o) in old["errors"]["details"].as_object().into_iter().flatten() {
        let n = &new["errors"]["details"][code];
        if n.is_null() {
            out.push(format!("error details for {code} removed"));
            continue;
        }
        if o["required"] != n["required"] {
            out.push(format!("error details for {code}: presence changed"));
        }
        c.node(&o["shape"], &n["shape"], &format!("error details {code}"), &mut out);
    }
    for (name, o) in old["streams"].as_object().into_iter().flatten() {
        let n = &new["streams"][name];
        if n.is_null() {
            out.push(format!("stream {name} removed"));
            continue;
        }
        if o["framing"] != n["framing"] {
            out.push(format!("stream {name}: framing changed"));
        }
        c.node(&o["event"], &n["event"], &format!("stream {name} event"), &mut out);
    }
    for key in old["limits"].as_object().into_iter().flatten().map(|(k, _)| k) {
        if new["limits"].get(key).is_none() {
            out.push(format!("limit {key} removed"));
        }
    }
    out
}

struct Cmp<'a> {
    old: &'a Value,
    new: &'a Value,
    /// (old definition, new definition) → why it is incompatible (empty: compatible). A pair
    /// under evaluation counts as compatible, which settles recursive types.
    memo: HashMap<(String, String), Vec<String>>,
}

fn reference(node: &Value) -> Option<&str> {
    node.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}
fn types(node: &Value) -> BTreeSet<String> {
    match node.get("type") {
        Some(Value::String(s)) => [s.clone()].into(),
        Some(Value::Array(a)) => a.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect(),
        _ => BTreeSet::new(),
    }
}
fn short(node: &Value) -> String {
    let mut s = node.to_string();
    if s.len() > 80 {
        s.truncate(s.floor_char_boundary(77));
        s.push_str("...");
    }
    s
}

impl Cmp<'_> {
    /// Whether `n` accepts every message `o` described with the same meaning; reasons into `out`.
    fn node(&mut self, o: &Value, n: &Value, path: &str, out: &mut Vec<String>) -> bool {
        if let (Some(or), Some(nr)) = (reference(o), reference(n)) {
            let key = (or.to_owned(), nr.to_owned());
            if let Some(reasons) = self.memo.get(&key) {
                out.extend(reasons.iter().cloned());
                return reasons.is_empty();
            }
            self.memo.insert(key.clone(), vec![]);
            let (od, nd) = (&self.old["$defs"][or], &self.new["$defs"][nr]);
            // reasons name the definition, so one root cause reads the same from every use
            let mut reasons = vec![];
            if nd.is_null() {
                reasons.push(format!("{or}: definition {nr} missing"));
            } else {
                self.node(od, nd, or, &mut reasons);
            }
            out.extend(reasons.iter().cloned());
            let ok = reasons.is_empty();
            self.memo.insert(key, reasons);
            return ok;
        }
        let before = out.len();
        let (Some(oo), Some(no)) = (o.as_object(), n.as_object()) else {
            if o != n {
                out.push(format!("{path}: changed {} → {}", short(o), short(n)));
            }
            return o == n;
        };
        let keys: BTreeSet<&String> = oo.keys().chain(no.keys()).filter(|k| !IGNORED.contains(&k.as_str())).collect();
        let empty = serde_json::Map::new();
        for key in keys {
            let (ov, nv) = (oo.get(key.as_str()), no.get(key.as_str()));
            match key.as_str() {
                "properties" => {
                    let (op, np) = (ov.and_then(Value::as_object).unwrap_or(&empty), nv.and_then(Value::as_object).unwrap_or(&empty));
                    for (name, schema) in op {
                        match np.get(name) {
                            None => out.push(format!("{path}.{name}: field removed")),
                            Some(next) => {
                                self.node(schema, next, &format!("{path}.{name}"), out);
                            }
                        }
                    }
                }
                "required" => {
                    let set = |v: Option<&Value>| -> BTreeSet<String> {
                        v.and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.as_str().map(str::to_owned)).collect()
                    };
                    let (or, nr) = (set(ov), set(nv));
                    for name in nr.difference(&or) {
                        out.push(format!("{path}.{name}: became required"));
                    }
                    let still = no.get("properties").and_then(Value::as_object).unwrap_or(&empty);
                    for name in or.difference(&nr).filter(|n| still.contains_key(*n)) {
                        out.push(format!("{path}.{name}: no longer always present"));
                    }
                }
                "anyOf" | "oneOf" | "allOf" => {
                    let (Some(ovs), Some(nvs)) = (ov.and_then(Value::as_array), nv.and_then(Value::as_array)) else {
                        out.push(format!("{path}: {key} {}", if ov.is_some() { "removed" } else { "added" }));
                        continue;
                    };
                    for (i, variant) in ovs.iter().enumerate() {
                        // the closest candidate's reasons say what broke, not only where
                        let mut closest: Option<Vec<String>> = None;
                        for candidate in nvs {
                            let mut reasons = vec![];
                            if self.node(variant, candidate, path, &mut reasons) {
                                closest = None;
                                break;
                            }
                            if closest.as_ref().is_none_or(|c| reasons.len() < c.len()) {
                                closest = Some(reasons);
                            }
                        }
                        if let Some(reasons) = closest {
                            out.push(format!("{path}: {key} variant {i} {} has no compatible counterpart", short(variant)));
                            out.extend(reasons);
                        }
                    }
                }
                "enum" => {
                    let nvs = nv.and_then(Value::as_array);
                    for value in ov.and_then(Value::as_array).into_iter().flatten() {
                        if !nvs.is_some_and(|n| n.contains(value)) {
                            out.push(format!("{path}: enum value {value} removed"));
                        }
                    }
                }
                "type" => {
                    if types(o) != types(n) {
                        out.push(format!("{path}: type {:?} → {:?}", types(o), types(n)));
                    }
                }
                "items" | "additionalProperties" => match (ov, nv) {
                    (Some(a), Some(b)) if a.is_object() && b.is_object() => {
                        self.node(a, b, &format!("{path}[]"), out);
                    }
                    // a closed object may open (new fields are additions); an open one may not close
                    (Some(Value::Bool(false)), Some(Value::Bool(true)) | None) if key == "additionalProperties" => {}
                    (a, b) if a != b => out.push(format!("{path}: {key} changed {} → {}", a.map_or("absent".into(), short), b.map_or("absent".into(), short))),
                    _ => {}
                },
                _ => {
                    if ov != nv {
                        out.push(format!("{path}: {key} changed {} → {}", ov.map_or("absent".into(), short), nv.map_or("absent".into(), short)));
                    }
                }
            }
        }
        out.len() == before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(defs: Value, methods: Value) -> Value {
        json!({"protocol": "sfp/1", "methods": methods, "$defs": defs, "errors": {"codes": ["busy"]}, "capabilities": {"offered": []}})
    }
    fn method(name: &str, params: &str) -> Value {
        json!({"name": name, "kind": "request", "capabilities": {}, "params": {"$ref": format!("#/$defs/{params}")}, "result": {"type": "object", "additionalProperties": false}})
    }
    fn base() -> Value {
        doc(
            json!({
                "Get": {"type": "object", "properties": {"id": {"type": "string"}, "kind": {"$ref": "#/$defs/Kind"}, "note": {"type": ["string", "null"]}}, "required": ["id", "kind"]},
                "Kind": {"oneOf": [{"type": "string", "const": "a"}, {"type": "string", "const": "b"}]},
            }),
            json!([method("_sill/x.get", "Get")]),
        )
    }

    #[test]
    fn the_recorded_document_is_compatible_with_itself() {
        assert!(breaking_changes(&base(), &base()).is_empty());
    }

    #[test]
    fn additions_are_compatible() {
        let mut new = base();
        new["$defs"]["Get"]["properties"]["extra"] = json!({"type": "boolean"});
        new["$defs"]["Kind"]["oneOf"].as_array_mut().unwrap().push(json!({"type": "string", "const": "c"}));
        new["errors"]["codes"].as_array_mut().unwrap().push(json!("new_code"));
        new["$defs"]["Get"]["properties"]["id"]["description"] = json!("docs may change");
        let mut added = method("_sill/x.new", "Get");
        added["capabilities"] = json!({"x.new": 1});
        new["methods"].as_array_mut().unwrap().push(added);
        assert_eq!(breaking_changes(&base(), &new), Vec::<String>::new());
    }

    #[test]
    fn renumbered_definitions_are_not_a_change() {
        let mut new = base();
        let defs = new["$defs"].as_object_mut().unwrap();
        let get = defs.remove("Get").unwrap();
        defs.insert("Get2".into(), get);
        defs.insert("Get".into(), json!({"type": "integer"}));
        new["methods"][0]["params"]["$ref"] = json!("#/$defs/Get2");
        assert!(breaking_changes(&base(), &new).is_empty());
    }

    #[test]
    fn removals_retypes_and_new_requirements_break() {
        let cases: Vec<(&str, Box<dyn Fn(&mut Value)>)> = vec![
            (
                "field removed",
                Box::new(|d| {
                    d["$defs"]["Get"]["properties"].as_object_mut().unwrap().remove("note");
                }),
            ),
            (
                "became required",
                Box::new(|d| {
                    d["$defs"]["Get"]["required"].as_array_mut().unwrap().push(json!("note"));
                }),
            ),
            (
                "no longer always present",
                Box::new(|d| {
                    d["$defs"]["Get"]["required"] = json!(["id"]);
                }),
            ),
            (
                "type",
                Box::new(|d| {
                    d["$defs"]["Get"]["properties"]["note"]["type"] = json!("string");
                }),
            ),
            (
                "no compatible counterpart",
                Box::new(|d| {
                    d["$defs"]["Kind"]["oneOf"].as_array_mut().unwrap().pop();
                }),
            ),
            (
                "method removed",
                Box::new(|d| {
                    d["methods"] = json!([]);
                }),
            ),
            (
                "must require a capability",
                Box::new(|d| {
                    d["methods"].as_array_mut().unwrap().push(method("_sill/x.new", "Get"));
                }),
            ),
            (
                "error code busy removed",
                Box::new(|d| {
                    d["errors"]["codes"] = json!([]);
                }),
            ),
            (
                "protocol identity changed",
                Box::new(|d| {
                    d["protocol"] = json!("sfp/2");
                }),
            ),
        ];
        for (expected, change) in cases {
            let mut new = base();
            change(&mut new);
            let found = breaking_changes(&base(), &new);
            assert!(found.iter().any(|f| f.contains(expected)), "{expected}: {found:?}");
        }
    }

    #[test]
    fn recursive_types_terminate() {
        let tree = |d: &mut Value| d["$defs"]["Get"]["properties"]["child"] = json!({"$ref": "#/$defs/Get"});
        let (mut old, mut new) = (base(), base());
        tree(&mut old);
        tree(&mut new);
        assert!(breaking_changes(&old, &new).is_empty());
        new["$defs"]["Get"]["properties"].as_object_mut().unwrap().remove("id");
        assert!(!breaking_changes(&old, &new).is_empty());
    }
}
