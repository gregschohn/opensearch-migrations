//! Loads the orchestration contract fixtures in `tests/fixtures/orchestration/`.
//!
//! Each fixture is the capture-proxy pod the Kubernetes orchestration renders for one migration
//! spec, generated from the real config processor and `setupCapture` templates by
//! `orchestrationSpecs/packages/migration-workflow-templates/tests/captureProxyRustConfigContract.test.ts`.
//! Never edit one by hand: regenerate it there.
//!
//! The capture-config and capture-proxy contract tests both include this file with `#[path]`.

// Each including test uses a different subset.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use capture_config::EnvLookup;
use serde_json::Value;

/// The scenarios the generator writes. A fixture missing from disk is a failure, not a skip.
pub const SCENARIOS: &[&str] = &[
    "default-proxy",
    "existing-kafka-msk-iam",
    "existing-kafka-scram-sha-512",
    "existing-tls-secret-verified-source-existing-kafka",
    "mtls-client-ca-file-not-required",
    "mtls-inline-client-ca",
    "no-capture",
    "plaintext-listener-managed-kafka-without-auth",
    "rejected-set-header-in-name-colon-value-form",
    "suppression-set-header-and-tuning",
];

/// One rendered capture-proxy pod.
pub struct Fixture {
    /// The scenario name, which is the file stem.
    pub name: String,
    /// The whole fixture document.
    pub document: Value,
}

/// An environment built from the container's `env` entries.
#[derive(Debug, Clone, Default)]
pub struct FixtureEnv(pub BTreeMap<String, String>);

impl EnvLookup for FixtureEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// Loads every fixture in `dir`, sorted by name, and checks the set matches [`SCENARIOS`].
pub fn load_all(dir: &Path) -> Vec<Fixture> {
    let mut fixtures: Vec<Fixture> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| {
            panic!("fixture directory {} is unreadable: {error}", dir.display())
        })
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| load(&path))
        .collect();
    fixtures.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = fixtures
        .iter()
        .map(|fixture| fixture.name.as_str())
        .collect();
    assert_eq!(
        names,
        SCENARIOS,
        "the fixture set in {} changed",
        dir.display()
    );
    fixtures
}

fn load(path: &PathBuf) -> Fixture {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} is unreadable: {error}", path.display()));
    let document: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()));
    Fixture {
        name: path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("a UTF-8 file name")
            .to_owned(),
        document,
    }
}

impl Fixture {
    /// What the proxy must make of the configuration.
    pub fn expect(&self) -> &Value {
        &self.document["expect"]
    }

    /// Whether the proxy must accept the configuration.
    pub fn accepted(&self) -> bool {
        self.expect()["accepted"]
            .as_bool()
            .expect("expect.accepted is a boolean")
    }

    fn container(&self) -> &Value {
        &self.document["pod"]["container"]
    }

    /// The container's argument vector, which the image entrypoint receives unchanged.
    pub fn args(&self) -> Vec<String> {
        self.container()["args"]
            .as_array()
            .expect("container.args is a list")
            .iter()
            .map(|arg| arg.as_str().expect("every argument is a string").to_owned())
            .collect()
    }

    /// The decoded `---INLINE-JSON` document, from the arguments themselves.
    pub fn decoded_inline_json(&self) -> Value {
        let args = self.args();
        assert_eq!(
            args.len(),
            2,
            "{}: the Kubernetes path passes two arguments",
            self.name
        );
        assert_eq!(args[0], "---INLINE-JSON", "{}", self.name);
        let bytes = BASE64
            .decode(args[1].as_bytes())
            .unwrap_or_else(|error| panic!("{}: the payload is not base64: {error}", self.name));
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("{}: the payload is not JSON: {error}", self.name))
    }

    fn cluster_value(&self, kind: &str, name: &str, key: &str) -> Option<String> {
        self.document["cluster"][kind][name][key]
            .as_str()
            .map(str::to_owned)
    }

    /// The container environment as the kubelet would build it.
    ///
    /// A `secretKeyRef` or `configMapKeyRef` resolves from the fixture's simulated cluster. A
    /// missing reference marked optional leaves the variable unset; a missing required one is a pod
    /// that never starts, so it fails the test.
    pub fn container_env(&self) -> FixtureEnv {
        let mut env = BTreeMap::new();
        for entry in self.container()["env"]
            .as_array()
            .expect("container.env is a list")
        {
            let name = entry["name"].as_str().expect("env name").to_owned();
            if let Some(value) = entry.get("value") {
                env.insert(name, value.as_str().expect("env value").to_owned());
                continue;
            }
            let (kind, reference) = if let Some(reference) = entry["valueFrom"].get("secretKeyRef")
            {
                ("secrets", reference)
            } else if let Some(reference) = entry["valueFrom"].get("configMapKeyRef") {
                ("configMaps", reference)
            } else {
                panic!(
                    "{}: env {name} has an unsupported source: {entry}",
                    self.name
                );
            };
            let source = reference["name"].as_str().expect("reference name");
            let key = reference["key"].as_str().expect("reference key");
            let optional = reference["optional"].as_bool().unwrap_or(false);
            match self.cluster_value(kind, source, key) {
                Some(value) => {
                    env.insert(name, value);
                }
                None => assert!(
                    optional,
                    "{}: env {name} requires {kind} {source}/{key}, so the pod would not start",
                    self.name
                ),
            }
        }
        FixtureEnv(env)
    }

    /// The `ConfigMap` backing a container path, and the key under it, when the path is inside a
    /// `ConfigMap` volume mount.
    pub fn config_map_file(&self, container_path: &Path) -> Option<(String, String)> {
        let mounts = self.container()["volumeMounts"].as_array()?;
        let volumes = self.document["pod"]["volumes"].as_array()?;
        mounts.iter().find_map(|mount| {
            let mount_path = Path::new(mount["mountPath"].as_str()?);
            let key = container_path
                .strip_prefix(mount_path)
                .ok()?
                .to_str()?
                .to_owned();
            let volume_name = mount["name"].as_str()?;
            let volume = volumes
                .iter()
                .find(|v| v["name"].as_str() == Some(volume_name))?;
            Some((volume["configMap"]["name"].as_str()?.to_owned(), key))
        })
    }

    /// A file's contents as the container sees them, when a `ConfigMap` volume provides it.
    pub fn config_map_file_contents(&self, container_path: &Path) -> Option<String> {
        let (config_map, key) = self.config_map_file(container_path)?;
        self.cluster_value("configMaps", &config_map, &key)
    }
}
