//! The Kubernetes orchestration's proxy configuration, accepted by the Rust configuration loader.
//!
//! Every fixture in `tests/fixtures/orchestration/` is the capture-proxy container the orchestration
//! renders for one migration spec, produced from the real TypeScript config processor and
//! `setupCapture` templates (see `support/orchestration_fixture.rs`). This test hands each
//! container's exact argument vector and environment to [`ApplicationConfig::from_args`], the
//! entry point the binary uses, and checks that the proxy understands it the way the orchestration
//! means it: the `expect` block of each fixture.
//!
//! A change on either side that breaks the contract fails here: an orchestration field the proxy
//! rejects, a field it interprets differently, or a proxy change that stops accepting what the
//! orchestration generates.

#[path = "support/orchestration_fixture.rs"]
mod orchestration_fixture;

use std::path::Path;

use capture_config::{ApplicationConfig, CaptureSink, ClientAuth, EnvLookup, TrustMaterial};
use orchestration_fixture::{Fixture, FixtureEnv, load_all};
use serde_json::{Value, json};

fn fixtures() -> Vec<Fixture> {
    load_all(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/orchestration"))
}

fn optional_str(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |text| Value::String(text.to_owned()))
}

/// What the proxy made of the configuration, in the shape of the fixture's `expect` block.
fn observe(fixture: &Fixture, config: &ApplicationConfig, env: &FixtureEnv) -> Value {
    let destination = config.destination();
    let tls = config.tls().map_or(Value::Null, |tls| {
        let trust = match tls.trust() {
            None => Value::Null,
            Some(TrustMaterial::InlinePem { env_var, pem }) => {
                assert_eq!(
                    Some(pem.expose().to_owned()),
                    env.get(env_var),
                    "{}: the trusted PEM must be the one the pod's environment carries",
                    fixture.name
                );
                json!({ "envVar": env_var })
            }
            Some(TrustMaterial::File(path)) => match fixture.config_map_file(path) {
                Some((config_map, key)) => json!({ "fileFromConfigMap": config_map, "key": key }),
                None => json!({ "unmountedFile": path.display().to_string() }),
            },
        };
        json!({
            "certChainFile": tls.cert_chain_file().display().to_string(),
            "keyFile": tls.key_file().display().to_string(),
            "clientAuth": match tls.client_auth() {
                ClientAuth::None => "none",
                ClientAuth::Require => "require",
            },
            "trust": trust,
        })
    });
    let capture = match config.sink() {
        CaptureSink::Disabled => json!("disabled"),
        CaptureSink::Kafka(kafka) => json!({
            "brokers": kafka.brokers(),
            "topic": kafka.topic(),
            "clientId": kafka.client_id(),
            "auth": kafka.auth().as_str(),
            "username": optional_str(kafka.username()),
            "passwordSet": kafka.password().is_some(),
            "listenerName": optional_str(kafka.listener_name()),
            "propertyFile": optional_str(kafka.property_file().and_then(Path::to_str)),
        }),
    };
    let suppression = config.suppression();
    let pattern = |pattern: Option<&capture_config::SuppressionPattern>| {
        optional_str(pattern.map(capture_config::SuppressionPattern::source))
    };
    let mut observed = json!({
        "accepted": true,
        "listenPort": config.listen_port(),
        "destination": {
            "uri": format!("{}://{}", destination.scheme().as_str(), destination.authority()),
            "insecure": config.insecure_destination(),
        },
        "tls": tls,
        "capture": capture,
        "suppression": {
            "method": pattern(suppression.method.as_ref()),
            "uriPath": pattern(suppression.uri_path.as_ref()),
            "methodAndPath": pattern(suppression.method_and_path.as_ref()),
            "headers": suppression
                .headers
                .iter()
                .map(|rule| json!([rule.name, rule.pattern.source()]))
                .collect::<Vec<_>>(),
        },
        "setHeader": config
            .set_header()
            .iter()
            .map(|pair| json!([pair.name, pair.value]))
            .collect::<Vec<_>>(),
        "telemetry": {
            "metrics": optional_str(config.telemetry().metrics.as_deref()),
            "traces": optional_str(config.telemetry().traces.as_deref()),
        },
        "workerThreads": config.worker_threads(),
        "maxTrafficBufferSize": config.max_traffic_buffer_size(),
        "connectionPool": {
            "size": config.connection_pool().size,
            "recycleAfterSeconds": config.connection_pool().recycle_after.as_secs(),
        },
    });
    if fixture.expect().get("awsRegion").is_some() {
        // MSK IAM signs for a region the proxy reads from AWS_REGION / AWS_DEFAULT_REGION.
        observed["awsRegion"] = optional_str(
            env.get("AWS_REGION")
                .or_else(|| env.get("AWS_DEFAULT_REGION"))
                .as_deref(),
        );
    }
    observed
}

/// Checks one fixture, returning a description of every way it fails.
fn check(fixture: &Fixture) -> Vec<String> {
    let mut failures = Vec::new();

    // The fixture's readable copy of the document must be the document the pod actually carries.
    if fixture.decoded_inline_json() != fixture.document["decodedInlineJson"] {
        failures.push(
            "decodedInlineJson does not match the container's ---INLINE-JSON payload".to_owned(),
        );
    }

    let env = fixture.container_env();
    let result = ApplicationConfig::from_args(&fixture.args(), &env);
    let expect = fixture.expect();
    match (fixture.accepted(), result) {
        (true, Ok(config)) => {
            let observed = observe(fixture, &config, &env);
            if &observed != expect {
                failures.push(format!(
                    "interpreted differently from what the orchestration means\n  expected: {expect}\n  observed: {observed}"
                ));
            }
        }
        (true, Err(error)) => failures.push(format!(
            "rejected (exit {}) a configuration the orchestration generates: {error}",
            error.exit_code()
        )),
        (false, Ok(config)) => failures.push(format!(
            "accepted a configuration that must be rejected: {config:?}"
        )),
        (false, Err(error)) => {
            let exit_code = i64::from(error.exit_code());
            if expect["exitCode"].as_i64() != Some(exit_code) {
                failures.push(format!(
                    "exited {exit_code}, expected {}: {error}",
                    expect["exitCode"]
                ));
            }
            let needle = expect["errorContains"].as_str().unwrap_or_default();
            if !error.to_string().contains(needle) {
                failures.push(format!("error does not mention {needle:?}: {error}"));
            }
        }
    }
    failures
}

#[test]
fn the_proxy_accepts_and_understands_every_orchestration_rendering() {
    let failures: Vec<String> = fixtures()
        .iter()
        .flat_map(|fixture| {
            check(fixture)
                .into_iter()
                .map(move |failure| format!("{}: {failure}", fixture.name))
        })
        .collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn every_document_key_the_orchestration_emits_is_a_declared_parameter() {
    // A misspelled or retired key is rejected as a whole-document error; naming the key here
    // makes the failure point at the orchestration field rather than at a scenario.
    for fixture in fixtures() {
        let document = fixture.decoded_inline_json();
        for key in document.as_object().expect("an object").keys() {
            assert!(
                capture_config::cli::parameter_for_json_key(key).is_some(),
                "{}: the orchestration emits {key:?}, which the proxy does not declare",
                fixture.name
            );
        }
    }
}

#[test]
fn accepted_configurations_exit_zero_and_rejected_ones_report_a_configuration_error() {
    for fixture in fixtures() {
        let result = ApplicationConfig::from_args(&fixture.args(), &fixture.container_env());
        if fixture.accepted() {
            assert!(result.is_ok(), "{}", fixture.name);
        } else {
            let error = result.expect_err(&fixture.name);
            assert_eq!(
                error.exit_code(),
                capture_config::CONFIG_ERROR_EXIT_CODE,
                "{}: {error}",
                fixture.name
            );
        }
    }
}
