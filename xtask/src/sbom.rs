//! Validate the Rust dependency inventory of a shipped binary.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use serde_json::Value;

const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Package {
    name: String,
    version: String,
}

pub(super) struct Inventory {
    root: Package,
    direct: BTreeSet<Package>,
    locked: BTreeSet<Package>,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| {
            !text.is_empty() && text.len() <= 1024 && !text.chars().any(char::is_control)
        })
        .ok_or_else(|| format!("missing or invalid string: {key}"))
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("missing or invalid array: {key}"))
}

fn package(value: &Value) -> Result<Package, String> {
    Ok(Package {
        name: text(value, "name")?.to_owned(),
        version: text(value, "version")?.to_owned(),
    })
}

/// Select the root and its locked, default-feature runtime dependencies from Cargo metadata.
pub(super) fn expected_inventory(metadata: &Value) -> Result<Inventory, String> {
    let mut packages = BTreeMap::new();
    let mut root = None;
    for value in array(metadata, "packages")? {
        let id = text(value, "id")?;
        let package = package(value)?;
        if package.name == "atx" && root.replace((id, package.clone())).is_some() {
            return Err("Cargo metadata contains multiple atx roots".into());
        }
        if packages.insert(id, package).is_some() {
            return Err("Cargo metadata contains duplicate package IDs".into());
        }
    }
    let (root_id, root) = root.ok_or("Cargo metadata has no atx root")?;
    let resolve = metadata
        .get("resolve")
        .ok_or("Cargo metadata has no resolve graph")?;
    let mut direct = BTreeSet::new();
    let mut found = false;
    for node in array(resolve, "nodes")? {
        if text(node, "id")? != root_id {
            continue;
        }
        if std::mem::replace(&mut found, true) {
            return Err("Cargo metadata contains duplicate root nodes".into());
        }
        for dependency in array(node, "deps")? {
            let kinds = array(dependency, "dep_kinds")?;
            if kinds.is_empty() {
                return Err("Cargo dependency has no resolved kinds".into());
            }
            let mut runtime = false;
            for kind in kinds {
                match kind.get("kind") {
                    Some(Value::Null) => runtime = true,
                    Some(Value::String(value)) if matches!(value.as_str(), "dev" | "build") => {}
                    _ => return Err("Cargo dependency has an invalid kind".into()),
                }
            }
            if runtime {
                let id = text(dependency, "pkg")?;
                direct.insert(
                    packages
                        .get(id)
                        .ok_or("unresolved Cargo runtime dependency")?
                        .clone(),
                );
            }
        }
    }
    if !found || direct.is_empty() {
        return Err("Cargo metadata has no resolved runtime dependencies".into());
    }
    Ok(Inventory {
        root,
        direct,
        locked: packages.into_values().collect(),
    })
}

/// Reject empty, mismatched, or disconnected binary inventories before artifact upload.
pub(super) fn validate(document: &Value, expected: &Inventory) -> Result<usize, String> {
    if text(document, "bomFormat")? != "CycloneDX"
        || text(document, "specVersion")? != "1.6"
        || document
            .get("version")
            .and_then(Value::as_u64)
            .is_none_or(|value| value == 0)
    {
        return Err("expected a CycloneDX 1.6 document with a positive revision".into());
    }
    let components = array(document, "components")?;
    if components.len() < 2 {
        return Err("SBOM must contain the atx root and non-empty dependencies".into());
    }
    let mut references = BTreeSet::new();
    let mut by_package = BTreeMap::new();
    for component in components {
        let reference = text(component, "bom-ref")?;
        if !references.insert(reference) {
            return Err(format!("duplicate SBOM reference: {reference}"));
        }
        if component.get("type").and_then(Value::as_str) == Some("file") {
            text(component, "name")?;
            continue;
        }
        let package = package(component)?;
        if !expected.locked.contains(&package) {
            return Err(format!(
                "unlocked Cargo component: {} {}",
                package.name, package.version
            ));
        }
        let purl = text(component, "purl")?;
        let expected_purl = format!(
            "pkg:cargo/{}@{}",
            package.name,
            package.version.replace('+', "%2B")
        );
        if purl != expected_purl {
            return Err(format!("invalid Cargo purl for {}", package.name));
        }
        if by_package.insert(package, reference).is_some() {
            return Err("duplicate Cargo package identity in SBOM".into());
        }
    }
    let root_ref = by_package
        .get(&expected.root)
        .ok_or("SBOM is missing the expected atx version")?;
    let mut graph = BTreeMap::new();
    for dependency in array(document, "dependencies")? {
        let reference = text(dependency, "ref")?;
        if !references.contains(reference) {
            return Err(format!("unknown dependency node: {reference}"));
        }
        let mut edges = BTreeSet::new();
        for edge in array(dependency, "dependsOn")? {
            let value = edge
                .as_str()
                .filter(|text| {
                    !text.is_empty() && text.len() <= 1024 && !text.chars().any(char::is_control)
                })
                .ok_or("dependency references must be bounded printable strings")?;
            if !references.contains(value) || value == reference || !edges.insert(value) {
                return Err(format!("invalid dependency edge: {reference} -> {value}"));
            }
        }
        if graph.insert(reference, edges).is_some() {
            return Err(format!("duplicate dependency node: {reference}"));
        }
    }
    let edges = graph
        .get(root_ref)
        .ok_or("SBOM has no root dependency graph")?;
    if edges.len() != expected.direct.len() {
        return Err("SBOM root dependency set differs from the default runtime graph".into());
    }
    for package in &expected.direct {
        let reference = by_package.get(package).ok_or_else(|| {
            format!(
                "SBOM is missing locked runtime dependency {} {}",
                package.name, package.version
            )
        })?;
        if !edges.contains(reference) {
            return Err(format!("SBOM root does not depend on {}", package.name));
        }
    }
    Ok(by_package.len())
}

/// Load bounded JSON documents so malformed artifacts fail rather than exhaust the gate.
fn read_json(path: &Path) -> Result<Value, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err(format!("not a regular JSON file: {}", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if u64::try_from(bytes.len()).map_err(|error| error.to_string())? > MAX_DOCUMENT_BYTES {
        return Err("JSON document exceeds the 16 MiB gate limit".into());
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

/// Validate a binary SBOM against metadata generated from this exact checkout and target.
pub(super) fn check(path: &Path, target: &str) -> Result<usize, String> {
    if !matches!(
        target,
        "aarch64-apple-darwin"
            | "x86_64-apple-darwin"
            | "x86_64-unknown-linux-gnu"
            | "x86_64-unknown-linux-musl"
            | "aarch64-unknown-linux-gnu"
            | "aarch64-unknown-linux-musl"
    ) {
        return Err(format!("unsupported release target: {target}"));
    }
    let output = std::process::Command::new("cargo")
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            target,
        ])
        .current_dir(super::workspace_root()?)
        .output()
        .map_err(|error| format!("cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let metadata: Value =
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    validate(&read_json(path)?, &expected_inventory(&metadata)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use serde_json::{Value, json};

    fn metadata() -> Value {
        json!({
            "packages": [
                {"id":"root", "name":"atx", "version":"0.1.1"},
                {"id":"clap", "name":"clap", "version":"4.6.6"},
                {"id":"libc", "name":"libc", "version":"0.2.189"},
                {"id":"test", "name":"proptest", "version":"1.11.0"}
            ],
            "resolve":{"nodes":[{"id":"root", "deps":[
                {"pkg":"clap", "dep_kinds":[{"kind":null}]},
                {"pkg":"libc", "dep_kinds":[{"kind":null}]},
                {"pkg":"test", "dep_kinds":[{"kind":"dev"}]}
            ]}]}
        })
    }

    fn document() -> Value {
        json!({
            "bomFormat":"CycloneDX", "specVersion":"1.6", "version":1,
            "components":[
                {"bom-ref":"root", "name":"atx", "version":"0.1.1", "purl":"pkg:cargo/atx@0.1.1"},
                {"bom-ref":"clap", "name":"clap", "version":"4.6.6", "purl":"pkg:cargo/clap@4.6.6"},
                {"bom-ref":"libc", "name":"libc", "version":"0.2.189", "purl":"pkg:cargo/libc@0.2.189"}
            ],
            "dependencies":[
                {"ref":"root", "dependsOn":["clap","libc"]},
                {"ref":"clap", "dependsOn":["libc"]},
                {"ref":"libc", "dependsOn":[]}
            ]
        })
    }

    #[test]
    fn binary_inventory_matches_the_locked_runtime_graph() {
        let expected = super::expected_inventory(&metadata()).expect("Cargo runtime inventory");
        assert_eq!(
            super::validate(&document(), &expected).expect("valid SBOM"),
            3
        );
    }

    #[test]
    fn scanner_file_evidence_does_not_replace_the_cargo_root() {
        let expected = super::expected_inventory(&metadata()).expect("Cargo runtime inventory");
        let mut doc = document();
        doc["components"]
            .as_array_mut()
            .expect("components")
            .push(json!({
                "bom-ref":"binary-hash", "type":"file", "name":"/atx"
            }));
        assert_eq!(super::validate(&doc, &expected).expect("file evidence"), 3);
        doc["components"][0] = json!({"bom-ref":"root", "type":"file", "name":"atx"});
        assert!(super::validate(&doc, &expected).is_err());
    }

    #[test]
    fn incomplete_or_wrong_binary_inventory_is_rejected() {
        let expected = super::expected_inventory(&metadata()).expect("Cargo runtime inventory");
        let base = document();
        let mut invalid = Vec::new();
        for value in [
            json!([]),
            json!([base["components"][0].clone()]),
            json!(null),
        ] {
            let mut doc = base.clone();
            doc["components"] = value;
            invalid.push(doc);
        }
        for (field, value) in [
            ("name", json!("another-app")),
            ("version", json!("0.1.0")),
            ("purl", json!("pkg:npm/atx@0.1.1")),
            ("bom-ref", json!(null)),
        ] {
            let mut doc = base.clone();
            doc["components"][0][field] = value;
            invalid.push(doc);
        }
        let mut wrong_version = base.clone();
        wrong_version["components"][1]["version"] = json!("4.0.0");
        invalid.push(wrong_version);
        let mut duplicate = base.clone();
        duplicate["components"][1]["bom-ref"] = json!("root");
        invalid.push(duplicate);
        for value in [json!([]), json!(null)] {
            let mut doc = base.clone();
            doc["dependencies"] = value;
            invalid.push(doc);
        }
        let mut missing_direct = base.clone();
        missing_direct["dependencies"][0]["dependsOn"] = json!(["clap"]);
        invalid.push(missing_direct);
        let mut dangling = base.clone();
        dangling["dependencies"][1]["dependsOn"] = json!(["unknown"]);
        invalid.push(dangling);
        let mut controls = base.clone();
        controls["dependencies"][1]["dependsOn"] = json!(["\u{1b}[2J"]);
        invalid.push(controls);
        let mut wrong_format = base.clone();
        wrong_format["bomFormat"] = json!("SPDX");
        invalid.push(wrong_format);
        for doc in invalid {
            assert!(super::validate(&doc, &expected).is_err(), "accepted {doc}");
        }
    }

    #[test]
    fn cargo_graph_requires_one_root_and_resolved_runtime_packages() {
        for value in [
            json!(null),
            json!({}),
            json!({"packages":[],"resolve":null}),
        ] {
            assert!(super::expected_inventory(&value).is_err());
        }
        let mut missing = metadata();
        missing["resolve"]["nodes"][0]["deps"][0]["pkg"] = json!("missing");
        assert!(super::expected_inventory(&missing).is_err());
        let mut empty = metadata();
        empty["resolve"]["nodes"][0]["deps"] = json!([]);
        assert!(super::expected_inventory(&empty).is_err());
        let mut kinds = metadata();
        kinds["resolve"]["nodes"][0]["deps"][0]["dep_kinds"] = json!([]);
        assert!(super::expected_inventory(&kinds).is_err());
        let mut duplicate = metadata();
        let root = duplicate["packages"][0].clone();
        duplicate["packages"]
            .as_array_mut()
            .expect("packages")
            .push(root);
        assert!(super::expected_inventory(&duplicate).is_err());
    }

    #[test]
    fn json_file_input_is_regular_valid_and_bounded() {
        use std::io::Write;

        struct OwnedFile(std::path::PathBuf);
        impl Drop for OwnedFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        let path = std::env::temp_dir().join(format!("atx-sbom-limit-{}.json", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("owned JSON fixture");
        let owned = OwnedFile(path);
        file.write_all(b"{}").expect("write JSON");
        assert_eq!(super::read_json(&owned.0).expect("read JSON"), json!({}));
        file.set_len(super::MAX_DOCUMENT_BYTES + 1)
            .expect("oversized sparse JSON");
        assert!(
            super::read_json(&owned.0)
                .expect_err("bounded input")
                .contains("16 MiB")
        );
        file.set_len(0).expect("truncate fixture");
        assert!(super::read_json(&owned.0).is_err());
        assert!(super::read_json(&std::env::temp_dir()).is_err());
        assert!(super::check(&owned.0, "unsupported-target").is_err());
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_json_shapes_do_not_panic(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4096)) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                let expected = super::expected_inventory(&metadata()).expect("fixture inventory");
                let _ = super::validate(&value, &expected);
                let _ = super::expected_inventory(&value);
            }
        }
    }
}
