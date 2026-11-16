//! The `---INLINE-JSON` / `---JSON-FILE` mechanism (spec §2).
//!
//! # Why this is the production path
//!
//! The Kubernetes/Argo path passes **exactly two** argv elements and no `--flag` at all
//! (`setupCapture.ts:741-744`):
//!
//! ```yaml
//! args:
//!   - ---INLINE-JSON
//!   - "{{=toBase64(inputs.parameters.jsonConfig)}}"
//! ```
//!
//! So this loader, not the flag surface, is what production configuration goes through. The flag
//! spellings matter only to the older CDK/ECS path, the dev stacks, and tests.
//!
//! # How it works here
//!
//! The JSON document is translated into an argument vector and handed to the same `clap` model the
//! flag path uses ([`crate::cli::CliArgs`]). One parser means one set of type coercions, one set of
//! defaults, and one set of error messages: the JSON and flag surfaces cannot drift.
//!
//! Precedence falls out of that (spec §2.4):
//!
//! ```text
//! JSON mode:  JSON key  >  CAPTURE_PROXY_* env var  >  default
//! Flag mode:  CLI flag  >  CAPTURE_PROXY_* env var  >  default
//! ```
//!
//! A key absent from the JSON is left to the environment or the default, because no argument is
//! emitted for it. A key **present** with a boolean `false` emits `--flag=false`, so it overrides
//! an environment variable that says true — which is what Java's unconditional field assignment
//! does (`JsonCommandLineParser.java:346-365`).
//!
//! Flag mode and JSON mode remain mutually exclusive, as in Java
//! (`JsonCommandLineParser.java:137-145, 220-224`): a deployment cannot mix the two.
//!
//! # Unknown keys are rejected
//!
//! `validateAllKeysAreParameters` (`JsonCommandLineParser.java:282-307`) rejects any key that is
//! not a declared parameter, and so does this. A misspelled key such as `supressCaptureForMethod`
//! must not be silently absorbed: silently ignoring it would leave capture enabled for traffic the
//! operator meant to suppress, or — for a misspelled `noCapture` — disable the configuration an
//! operator believes is in force.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;

use crate::cli::{ParamKind, parameter_for_json_key};

/// The inline-JSON argument, with three leading dashes and exact case
/// (`JsonCommandLineParser.java:49`).
pub const INLINE_JSON_FLAG: &str = "---INLINE-JSON";

/// The JSON-file argument (`JsonCommandLineParser.java:50`).
pub const JSON_FILE_FLAG: &str = "---JSON-FILE";

/// The reserved subcommand-selection key (`JsonCommandLineParser.java:51`).
///
/// The proxy registers no subcommands (`CaptureProxy.java:295` only calls `addObject`), so this key
/// is **accepted and ignored**, exactly as `validateAllKeysAreParameters` does. Rejecting it would
/// break an orchestration document that includes it.
pub const JCOMMANDER_COMMAND_KEY: &str = "jcommanderCommand";

/// Which configuration mode a set of arguments selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Ordinary `--flag value` arguments.
    Flags,
    /// A JSON document supplied inline.
    InlineJson,
    /// A JSON document read from a file.
    JsonFile {
        /// The file that was read.
        path: String,
    },
}

/// The argument vector to parse, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArgs {
    /// Arguments **excluding** the program name, ready for `clap`.
    pub args: Vec<String>,
    /// The mode that produced them.
    pub source: ConfigSource,
}

/// Resolves a raw argument vector into the arguments `clap` should parse.
///
/// `args` must **exclude** the program name. In flag mode the arguments are returned unchanged; in
/// JSON mode they are replaced by arguments synthesised from the document.
///
/// # Errors
///
/// Every variant of [`InlineJsonError`] is a configuration error: a JSON argument with no payload,
/// a JSON argument mixed with anything else, an unreadable file, an undecodable payload, a
/// non-object document, an unknown key, or a value whose shape does not fit its parameter.
pub fn resolve_args<S: AsRef<str>>(args: &[S]) -> Result<ResolvedArgs, InlineJsonError> {
    let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    let Some((index, flag)) = args.iter().enumerate().find_map(|(index, arg)| match *arg {
        INLINE_JSON_FLAG => Some((index, INLINE_JSON_FLAG)),
        JSON_FILE_FLAG => Some((index, JSON_FILE_FLAG)),
        _ => None,
    }) else {
        // No JSON argument: flag mode, including the empty argument vector.
        return Ok(ResolvedArgs {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            source: ConfigSource::Flags,
        });
    };

    let payload = args
        .get(index + 1)
        .ok_or(InlineJsonError::MissingPayload { flag })?;

    // JsonCommandLineParser.java:220-224 accepts only two or three arguments, and with no
    // subcommands registered the three-argument form is rejected at :249-252. So the proxy accepts
    // exactly ["---INLINE-JSON", "<payload>"] or ["---JSON-FILE", "<path>"] and nothing else.
    if args.len() != 2 {
        let unexpected = args
            .iter()
            .enumerate()
            .find(|(position, _)| *position != index && *position != index + 1)
            .map_or_else(String::new, |(_, arg)| (*arg).to_owned());
        return Err(InlineJsonError::UnexpectedArgument {
            flag,
            argument: unexpected,
        });
    }

    let (document, source) = if flag == INLINE_JSON_FLAG {
        (decode_payload(payload)?, ConfigSource::InlineJson)
    } else {
        let path = Path::new(payload);
        if !path.exists() {
            return Err(InlineJsonError::FileNotFound {
                path: payload.to_string(),
            });
        }
        let content =
            std::fs::read_to_string(path).map_err(|error| InlineJsonError::FileUnreadable {
                path: payload.to_string(),
                detail: error.to_string(),
            })?;
        (
            decode_payload(&content)?,
            ConfigSource::JsonFile {
                path: payload.to_string(),
            },
        )
    };

    Ok(ResolvedArgs {
        args: document_to_args(&document)?,
        source,
    })
}

/// Decodes a payload that is either JSON text or base64-encoded JSON text.
///
/// `OuterParsedArgs` (`JsonCommandLineParser.java:163-173`) trims the payload and uses it verbatim
/// when it starts with `{` or `[`, and base64-decodes it otherwise. The decode path logs
/// `Decoding argument as base64` and deliberately never logs the contents, because they may carry
/// credentials — a care `CaptureProxy.java:558` then undoes by echoing the raw argument.
///
/// # Errors
///
/// [`InlineJsonError::UndecodablePayload`] when the payload is neither JSON text nor valid base64,
/// and [`InlineJsonError::MalformedJson`] when the decoded text is not JSON.
pub fn decode_payload(payload: &str) -> Result<Value, InlineJsonError> {
    let trimmed = payload.trim();
    let text = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        payload.to_owned()
    } else {
        tracing::info!("event=capture_config_base64_decode");
        let bytes = BASE64.decode(trimmed.as_bytes()).map_err(|error| {
            InlineJsonError::UndecodablePayload {
                detail: error.to_string(),
            }
        })?;
        String::from_utf8(bytes).map_err(|error| InlineJsonError::UndecodablePayload {
            detail: error.to_string(),
        })?
    };
    serde_json::from_str(&text).map_err(|error| InlineJsonError::MalformedJson {
        detail: error.to_string(),
    })
}

/// Translates a configuration document into `clap` arguments.
///
/// # Errors
///
/// [`InlineJsonError::NotAnObject`] for a document that is not a JSON object,
/// [`InlineJsonError::UnrecognizedKeys`] for any key that is not a declared parameter, and
/// [`InlineJsonError::InvalidValue`] for a value whose shape does not fit its parameter.
pub fn document_to_args(document: &Value) -> Result<Vec<String>, InlineJsonError> {
    let object = document
        .as_object()
        .ok_or_else(|| InlineJsonError::NotAnObject {
            kind: json_kind(document),
        })?;

    let mut unrecognized = Vec::new();
    let mut args = Vec::new();

    for (key, value) in object {
        if key == JCOMMANDER_COMMAND_KEY {
            // Accepted and ignored: the proxy registers no subcommands.
            continue;
        }
        let Some(spec) = parameter_for_json_key(key) else {
            unrecognized.push(key.clone());
            continue;
        };
        let flag = format!("--{}", spec.flag);
        match spec.kind {
            ParamKind::Switch => {
                let set = value
                    .as_bool()
                    .ok_or_else(|| InlineJsonError::InvalidValue {
                        key: key.clone(),
                        detail: format!("expected a boolean, got {}", json_kind(value)),
                    })?;
                // The explicit form, so a JSON `false` overrides an environment variable.
                args.push(format!("{flag}={set}"));
            }
            ParamKind::Value => {
                args.push(flag);
                args.push(scalar_to_string(key, value)?);
            }
            ParamKind::Pair => {
                for pair in pairs_from_value(key, value)? {
                    args.push(flag.clone());
                    args.push(pair.0);
                    args.push(pair.1);
                }
            }
        }
    }

    if !unrecognized.is_empty() {
        unrecognized.sort();
        return Err(InlineJsonError::UnrecognizedKeys { keys: unrecognized });
    }
    Ok(args)
}

fn scalar_to_string(key: &str, value: &Value) -> Result<String, InlineJsonError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        other => Err(InlineJsonError::InvalidValue {
            key: key.to_owned(),
            detail: format!(
                "expected a string, number, or boolean, got {}",
                json_kind(other)
            ),
        }),
    }
}

/// Reads a pair-list value in any of the three accepted shapes.
///
/// * a flat, even-length array of strings — the shape Java's `List` path produces
///   (`JsonCommandLineParser.java:510-517`), which `convertPairListToMap` reads two at a time;
/// * an array of two-element arrays;
/// * an array of `{"name": …, "value": …}` objects.
///
/// An odd-length flat array is a configuration error. In Java it is an
/// `IndexOutOfBoundsException` thrown after `parseArgs` has already returned, so it escapes the
/// configuration-error handler entirely (spec discrepancy #18).
pub(crate) fn pairs_from_value(
    key: &str,
    value: &Value,
) -> Result<Vec<(String, String)>, InlineJsonError> {
    let invalid = |detail: String| InlineJsonError::InvalidValue {
        key: key.to_owned(),
        detail,
    };
    let array = value
        .as_array()
        .ok_or_else(|| invalid(format!("expected an array, got {}", json_kind(value))))?;

    if array.iter().all(Value::is_string) {
        if !array.len().is_multiple_of(2) {
            return Err(invalid(format!(
                "a flat array must hold a name followed by a value for each entry, so it needs an \
                 even number of elements; got {}",
                array.len()
            )));
        }
        return array
            .chunks_exact(2)
            .map(|pair| {
                let name = pair[0].as_str().unwrap_or_default().to_owned();
                let header_value = pair[1].as_str().unwrap_or_default().to_owned();
                if name.is_empty() {
                    return Err(invalid("a pair has an empty name".to_owned()));
                }
                Ok((name, header_value))
            })
            .collect();
    }

    array
        .iter()
        .map(|entry| match entry {
            Value::Array(pair) if pair.len() == 2 => {
                let name = pair[0]
                    .as_str()
                    .ok_or_else(|| invalid("a pair's name must be a string".to_owned()))?;
                let header_value = pair[1]
                    .as_str()
                    .ok_or_else(|| invalid("a pair's value must be a string".to_owned()))?;
                if name.is_empty() {
                    return Err(invalid("a pair has an empty name".to_owned()));
                }
                Ok((name.to_owned(), header_value.to_owned()))
            }
            Value::Object(fields) => {
                let name = fields
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("an object pair needs a string \"name\"".to_owned()))?;
                let header_value = fields
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("an object pair needs a string \"value\"".to_owned()))?;
                if name.is_empty() {
                    return Err(invalid("a pair has an empty name".to_owned()));
                }
                Ok((name.to_owned(), header_value.to_owned()))
            }
            other => Err(invalid(format!(
                "expected a two-element array or an object with \"name\" and \"value\", got {}",
                json_kind(other)
            ))),
        })
        .collect()
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// A configuration document could not be loaded.
///
/// No variant carries a payload or a decoded document: the document may contain credentials, and
/// `JsonCommandLineParser.java:171` deliberately avoids logging its contents.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InlineJsonError {
    /// The JSON argument had nothing after it.
    #[error("{flag} requires an argument immediately after it")]
    MissingPayload {
        /// Which JSON argument.
        flag: &'static str,
    },
    /// Something other than the JSON argument and its payload was present.
    #[error(
        "{flag} cannot be combined with other arguments; expected exactly \
         [{flag} <json-or-path>] but also got {argument:?}"
    )]
    UnexpectedArgument {
        /// Which JSON argument.
        flag: &'static str,
        /// The first extra argument.
        argument: String,
    },
    /// A `---JSON-FILE` path does not exist.
    #[error("JSON file not found: {path}")]
    FileNotFound {
        /// The path that was given.
        path: String,
    },
    /// A `---JSON-FILE` path exists but could not be read.
    #[error("JSON file {path} could not be read: {detail}")]
    FileUnreadable {
        /// The path that was given.
        path: String,
        /// The underlying I/O failure.
        detail: String,
    },
    /// The payload was neither JSON text nor valid base64.
    #[error("the configuration payload is neither JSON nor base64-encoded JSON: {detail}")]
    UndecodablePayload {
        /// Why decoding failed. Never the payload itself.
        detail: String,
    },
    /// The decoded payload was not JSON.
    #[error("the configuration document is not valid JSON: {detail}")]
    MalformedJson {
        /// The parse failure, which may include a position but never the document.
        detail: String,
    },
    /// The document was valid JSON but not an object.
    #[error("the configuration document must be a JSON object, got {kind}")]
    NotAnObject {
        /// What was found instead.
        kind: &'static str,
    },
    /// One or more keys are not declared parameters.
    #[error(
        "unrecognized parameter(s) in the configuration document: {}. \
         A misspelled key is rejected rather than ignored, because ignoring it would silently \
         change what is captured",
        keys.join(", ")
    )]
    UnrecognizedKeys {
        /// The offending keys, sorted.
        keys: Vec<String>,
    },
    /// A value's shape does not fit its parameter.
    #[error("the value of {key} is invalid: {detail}")]
    InvalidValue {
        /// The key whose value was rejected.
        key: String,
        /// What was expected.
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;
    use crate::cli::CliArgs;

    fn resolve(args: &[&str]) -> ResolvedArgs {
        resolve_args(args).expect("resolvable")
    }

    fn parse_json(document: &str) -> CliArgs {
        let resolved = resolve(&[INLINE_JSON_FLAG, document]);
        let mut argv = vec!["capture-proxy".to_owned()];
        argv.extend(resolved.args);
        CliArgs::try_parse_from(argv).expect("valid configuration")
    }

    #[test]
    fn no_json_argument_means_flag_mode() {
        let resolved = resolve(&["--listen-port", "9200"]);
        assert_eq!(resolved.source, ConfigSource::Flags);
        assert_eq!(resolved.args, ["--listen-port", "9200"]);
    }

    #[test]
    fn an_empty_argument_vector_is_flag_mode_with_nothing_set() {
        let empty: [&str; 0] = [];
        let resolved = resolve_args(&empty).expect("resolvable");
        assert_eq!(resolved.source, ConfigSource::Flags);
        assert!(resolved.args.is_empty());
    }

    #[test]
    fn the_production_invocation_shape_is_accepted() {
        // Exactly what setupCapture.ts:741-744 emits: two argv elements, base64 payload.
        let document = r#"{"listenPort":9200,"destinationUri":"https://source:9200"}"#;
        let payload = BASE64.encode(document);
        let args = parse_json(&payload);
        assert_eq!(args.listen_port, 9200);
        assert_eq!(args.destination_uri, "https://source:9200");
    }

    #[test]
    fn a_plain_json_payload_is_used_verbatim() {
        let args = parse_json(r#"{"listenPort":9200,"destinationUri":"http://h:9200"}"#);
        assert_eq!(args.listen_port, 9200);
    }

    #[test]
    fn a_payload_with_surrounding_whitespace_is_still_recognised_as_json() {
        // JsonCommandLineParser.java:164-167 trims before testing the first character.
        let args = parse_json("  \n{\"listenPort\":9200,\"destinationUri\":\"http://h:9200\"}\n");
        assert_eq!(args.listen_port, 9200);
    }

    #[test]
    fn three_leading_dashes_and_exact_case_are_required() {
        // "--INLINE-JSON" and "---inline-json" are not the JSON argument; they stay flag-mode
        // arguments and clap rejects them as unknown.
        for spelling in ["--INLINE-JSON", "---inline-json", "---INLINE_JSON"] {
            let resolved = resolve(&[spelling, "{}"]);
            assert_eq!(resolved.source, ConfigSource::Flags, "{spelling}");
        }
    }

    #[test]
    fn a_json_argument_with_no_payload_is_rejected() {
        assert_eq!(
            resolve_args(&[INLINE_JSON_FLAG]).expect_err("no payload"),
            InlineJsonError::MissingPayload {
                flag: INLINE_JSON_FLAG
            }
        );
        assert_eq!(
            resolve_args(&[JSON_FILE_FLAG]).expect_err("no payload"),
            InlineJsonError::MissingPayload {
                flag: JSON_FILE_FLAG
            }
        );
    }

    #[test]
    fn json_mode_cannot_be_mixed_with_flags() {
        // JsonCommandLineParser.java:220-224 plus :249-252 — with no subcommands registered, a
        // third argument is always an error.
        let error = resolve_args(&[INLINE_JSON_FLAG, "{}", "--listen-port"])
            .expect_err("a flag may not accompany JSON");
        assert!(matches!(error, InlineJsonError::UnexpectedArgument { .. }));
        let error = resolve_args(&["--listen-port", "9200", INLINE_JSON_FLAG, "{}"])
            .expect_err("a flag may not accompany JSON");
        assert!(matches!(error, InlineJsonError::UnexpectedArgument { .. }));
    }

    #[test]
    fn an_undecodable_payload_is_rejected_without_echoing_it() {
        let error = resolve_args(&[INLINE_JSON_FLAG, "not base64 and not json!!"])
            .expect_err("undecodable");
        assert!(matches!(error, InlineJsonError::UndecodablePayload { .. }));
        assert!(
            !error.to_string().contains("not base64 and not json"),
            "the payload must not appear in the error: {error}"
        );
    }

    #[test]
    fn base64_of_something_that_is_not_json_is_rejected() {
        let payload = BASE64.encode("listenPort=9200");
        let error = resolve_args(&[INLINE_JSON_FLAG, &payload]).expect_err("not json");
        assert!(matches!(error, InlineJsonError::MalformedJson { .. }));
    }

    #[test]
    fn a_document_that_is_not_an_object_is_rejected() {
        // Java reads field names off the root node, so an array sets nothing at all and then fails
        // on the missing required parameters. Saying so plainly is better.
        let error = resolve_args(&[INLINE_JSON_FLAG, "[1,2,3]"]).expect_err("not an object");
        assert!(matches!(error, InlineJsonError::NotAnObject { .. }));
    }

    #[test]
    fn an_unknown_key_is_rejected_not_ignored() {
        // validateAllKeysAreParameters, JsonCommandLineParser.java:282-307.
        let error = resolve_args(&[
            INLINE_JSON_FLAG,
            r#"{"listenPort":9200,"supressCaptureForMethod":"GET"}"#,
        ])
        .expect_err("a misspelled key must be rejected");
        match error {
            InlineJsonError::UnrecognizedKeys { keys } => {
                assert_eq!(keys, ["supressCaptureForMethod"]);
            }
            other => panic!("expected an unrecognized-key error, got {other}"),
        }
    }

    #[test]
    fn the_reserved_jcommander_command_key_is_accepted_and_ignored() {
        // JsonCommandLineParser.java:298 — the proxy registers no subcommands, so the key is inert
        // rather than invalid.
        let args = parse_json(
            r#"{"jcommanderCommand":"anything","listenPort":9200,"destinationUri":"http://h:9200"}"#,
        );
        assert_eq!(args.listen_port, 9200);
    }

    #[test]
    fn both_the_dashed_and_the_camel_spelling_of_a_key_work() {
        // JsonCommandLineParser.java:395-398 registers both per declared name.
        let camel = parse_json(
            r#"{"listenPort":9200,"destinationUri":"http://h:9200",
                "maxConnectionDurationSeconds":120}"#,
        );
        let dashed = parse_json(
            r#"{"listenPort":9200,"destinationUri":"http://h:9200",
                "max-connection-duration-seconds":120}"#,
        );
        assert_eq!(camel.max_connection_duration_seconds, 120);
        assert_eq!(dashed.max_connection_duration_seconds, 120);
    }

    #[test]
    fn either_alias_of_a_multi_named_parameter_resolves() {
        for key in ["kafkaBrokers", "kafkaConnection", "kafka-brokers"] {
            let document = format!(
                r#"{{"listenPort":9200,"destinationUri":"http://h:9200","{key}":"broker:9092"}}"#
            );
            assert_eq!(
                parse_json(&document).kafka_brokers.as_deref(),
                Some("broker:9092"),
                "{key}"
            );
        }
        for key in ["kafkaPropertyFile", "kafkaConfigFile"] {
            let document = format!(
                r#"{{"listenPort":9200,"destinationUri":"http://h:9200","{key}":"/config/k.properties"}}"#
            );
            assert_eq!(
                parse_json(&document).kafka_property_file,
                Some(std::path::PathBuf::from("/config/k.properties")),
                "{key}"
            );
        }
    }

    #[test]
    fn a_key_absent_from_the_document_keeps_its_default() {
        let args = parse_json(r#"{"listenPort":9200,"destinationUri":"http://h:9200"}"#);
        assert_eq!(args.kafka_topic, "logging-traffic-topic");
        assert_eq!(args.max_incomplete_request_total_bytes, 262_144_000);
    }

    #[test]
    fn a_boolean_false_is_emitted_explicitly_so_it_overrides_an_environment_variable() {
        // Java's populateFromJson assigns unconditionally, so a JSON false beats an env var that
        // says true. Emitting a bare switch would silently lose that.
        let resolved = resolve(&[
            INLINE_JSON_FLAG,
            r#"{"noCapture":false,"insecureDestination":true}"#,
        ]);
        assert!(resolved.args.contains(&"--no-capture=false".to_owned()));
        assert!(
            resolved
                .args
                .contains(&"--insecure-destination=true".to_owned())
        );
    }

    #[test]
    fn a_non_boolean_switch_value_is_rejected() {
        let error = resolve_args(&[INLINE_JSON_FLAG, r#"{"noCapture":"yes"}"#])
            .expect_err("a switch takes a boolean");
        assert!(matches!(error, InlineJsonError::InvalidValue { .. }));
    }

    #[test]
    fn numbers_and_strings_both_coerce_for_a_scalar_parameter() {
        let as_number = parse_json(r#"{"listenPort":9200,"destinationUri":"http://h:9200"}"#);
        let as_string = parse_json(r#"{"listenPort":"9200","destinationUri":"http://h:9200"}"#);
        assert_eq!(as_number.listen_port, as_string.listen_port);
    }

    #[test]
    fn a_flat_even_header_array_is_read_pairwise_like_java() {
        let args = parse_json(
            r#"{"listenPort":9200,"destinationUri":"http://h:9200",
                "setHeader":["X-A","1","X-B","2"]}"#,
        );
        assert_eq!(args.set_header, ["X-A", "1", "X-B", "2"]);
    }

    #[test]
    fn an_odd_header_array_is_a_configuration_error_rather_than_a_crash() {
        // Spec discrepancy #18: Java throws IndexOutOfBoundsException at CaptureProxy.java:517.
        let error = resolve_args(&[
            INLINE_JSON_FLAG,
            r#"{"setHeader":["Authorization: Basic abc"]}"#,
        ])
        .expect_err("an odd-length array must be rejected");
        match error {
            InlineJsonError::InvalidValue { key, detail } => {
                assert_eq!(key, "setHeader");
                assert!(detail.contains("even number"), "{detail}");
            }
            other => panic!("expected an invalid-value error, got {other}"),
        }
    }

    #[test]
    fn the_explicit_pair_shapes_are_accepted_too() {
        let nested = parse_json(
            r#"{"listenPort":9200,"destinationUri":"http://h:9200",
                "setHeader":[["X-A","1"],["X-B","2"]]}"#,
        );
        assert_eq!(nested.set_header, ["X-A", "1", "X-B", "2"]);
        let objects = parse_json(
            r#"{"listenPort":9200,"destinationUri":"http://h:9200",
                "setHeader":[{"name":"X-A","value":"1"},{"name":"X-B","value":"2"}]}"#,
        );
        assert_eq!(objects.set_header, ["X-A", "1", "X-B", "2"]);
    }

    #[test]
    fn an_empty_header_array_is_accepted() {
        // The orchestration always emits suppressCaptureForHeaderMatch, defaulted to [].
        let args = parse_json(
            r#"{"listenPort":9200,"destinationUri":"http://h:9200",
                "suppressCaptureForHeaderMatch":[]}"#,
        );
        assert!(args.suppress_capture_for_header_match.is_empty());
    }

    #[test]
    fn a_header_value_containing_a_comma_survives() {
        // Java declares both flags with NoSplitter for exactly this reason.
        let args = parse_json(
            r#"{"listenPort":9200,"destinationUri":"http://h:9200",
                "setHeader":["X-A","one,two"]}"#,
        );
        assert_eq!(args.set_header, ["X-A", "one,two"]);
    }

    #[test]
    fn a_json_file_is_read_and_may_itself_be_base64() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plain = dir.path().join("plain.json");
        std::fs::write(
            &plain,
            r#"{"listenPort":9200,"destinationUri":"http://h:9200"}"#,
        )
        .expect("write");
        let resolved = resolve(&[JSON_FILE_FLAG, plain.to_str().expect("utf-8")]);
        assert!(matches!(resolved.source, ConfigSource::JsonFile { .. }));
        assert!(resolved.args.contains(&"--listen-port".to_owned()));

        let encoded = dir.path().join("encoded.b64");
        std::fs::write(
            &encoded,
            BASE64.encode(r#"{"listenPort":9201,"destinationUri":"http://h:9200"}"#),
        )
        .expect("write");
        let resolved = resolve(&[JSON_FILE_FLAG, encoded.to_str().expect("utf-8")]);
        assert!(resolved.args.contains(&"9201".to_owned()));
    }

    #[test]
    fn a_missing_json_file_is_rejected() {
        let error = resolve_args(&[JSON_FILE_FLAG, "/nonexistent/proxy-config.json"])
            .expect_err("a missing file is not an empty configuration");
        assert!(matches!(error, InlineJsonError::FileNotFound { .. }));
    }

    #[test]
    fn the_whole_orchestration_document_round_trips() {
        // Spec §10.1's complete emitted key set, in the SCRAM + TLS shape.
        let document = r#"{
            "listenPort": 9200,
            "destinationUri": "https://source-cluster:9200",
            "insecureDestination": false,
            "noCapture": false,
            "kafkaConnection": "b-1:9096,b-2:9096",
            "kafkaTopic": "logging-traffic-topic",
            "kafkaListenerName": "CLIENT_SASL_SCRAM",
            "kafkaAuthType": "scram-sha-512",
            "kafkaSecretName": "proxy-kafka-user",
            "kafkaUserName": "capture",
            "kafkaClientId": "HttpCaptureProxyProducer",
            "kafkaPropertyFile": "/config/kafka-auth/client.properties",
            "enableMSKAuth": false,
            "numThreads": 0,
            "maxTrafficBufferSize": 1048576,
            "destinationConnectionPoolSize": 0,
            "destinationConnectionPoolTimeout": "PT30S",
            "suppressCaptureForHeaderMatch": [],
            "suppressCaptureForMethod": "",
            "suppressCaptureForUriPath": "",
            "suppressMethodAndPath": "",
            "otelMetricsCollectorEndpoint": "http://otel-collector:4317",
            "sslCertChainFile": "/etc/proxy-tls/tls.crt",
            "sslKeyFile": "/etc/proxy-tls/tls.key",
            "sslTrustCertPemEnvVar": "CAPTURE_PROXY_SSL_TRUST_CERT_PEM",
            "requireClientAuth": true
        }"#;
        let args = parse_json(document);
        assert_eq!(args.listen_port, 9200);
        assert_eq!(args.destination_uri, "https://source-cluster:9200");
        assert!(!args.no_capture);
        assert_eq!(args.kafka_brokers.as_deref(), Some("b-1:9096,b-2:9096"));
        assert_eq!(args.kafka_auth_type.as_deref(), Some("scram-sha-512"));
        assert!(!args.enable_msk_auth);
        assert!(args.require_client_auth);
        assert_eq!(
            args.ssl_trust_cert_pem_env_var.as_deref(),
            Some("CAPTURE_PROXY_SSL_TRUST_CERT_PEM")
        );
        assert_eq!(args.suppress_capture_for_method.as_deref(), Some(""));
        assert_eq!(
            args.otel_metrics_collector_endpoint.as_deref(),
            Some("http://otel-collector:4317")
        );
    }
}
