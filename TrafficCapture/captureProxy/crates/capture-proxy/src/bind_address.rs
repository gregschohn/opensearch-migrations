//! The `--bind-address` parameter, and why it is parsed here rather than in `capture-config`.
//!
//! # Exposure
//!
//! [`capture_transport::ListenerConfig`] defaults to a loopback bind and reaches the wildcard only
//! through `with_bind_address`, deliberately, so that choosing the wildcard is a visible act. This
//! module is that visible act: the default here is [`DEFAULT_BIND_ADDRESS`] (`0.0.0.0`), which is
//! Java parity — `NettyScanningHttpProxy.start` calls `bind(int port)` and `CaptureProxy` has no
//! bind-address parameter at all.
//!
//! That default is compliant rather than inherited. The proxy's deployed shape is a Kubernetes
//! Service whose load balancer is **internal by default**: the orchestration schema declares
//! `internetFacing: z.boolean().default(false)`
//! (`orchestrationSpecs/packages/schemas/src/userSchemas.ts:716`) and the generated Service
//! annotates `aws-load-balancer-scheme: internal`, `networking.gke.io/load-balancer-type:
//! Internal`, or `azure-load-balancer-internal: true` accordingly
//! (`setupCapture.ts:82-115`). The container image publishes no port and declares no `EXPOSE`. So
//! the pod network is the private network, the Service publish is the real trust boundary, and
//! narrowing the in-container bind would stop the proxy serving the cluster it exists to sit in
//! front of.
//!
//! What the bind does *not* do is authenticate. The proxy's only caller gate is frontside mTLS
//! (`--require-client-auth`); with it off, reachability on the pod network is the whole
//! authorisation story. [`describe_exposure`] is what says so at start-up, every time, so an
//! operator who narrowed nothing still sees what they are running.
//!
//! # Why the parameter is extracted from `argv` by hand
//!
//! `capture_config::CliArgs` is the frozen parameter model and this is a new parameter that Java
//! has no spelling for, so adding it there would change a contract this crate does not own. It also
//! could not be routed through the JSON path even if it were added: `resolve_args` accepts
//! **exactly** `["---INLINE-JSON", "<payload>"]` or `["---JSON-FILE", "<path>"]` and rejects any
//! third argument (`inline_json.rs:120-132`), and an unrecognised JSON key is a hard error
//! (`document_to_args`). A deployment on the Kubernetes/Argo path therefore has no `--flag` to
//! carry this at all, which is why [`BIND_ADDRESS_ENV`] exists alongside the flag.
//!
//! DEVIATION: this parameter does not exist in Java, and it is the only configuration input the
//! proxy reads outside `capture-config`.

use std::net::{IpAddr, Ipv4Addr};

/// The flag's canonical spelling.
pub const BIND_ADDRESS_FLAG: &str = "--bind-address";

/// Environment variable carrying the same value.
///
/// The only way to set the bind address on the inline-JSON path, which admits no extra argument.
pub const BIND_ADDRESS_ENV: &str = "CAPTURE_PROXY_BIND_ADDRESS";

/// The default bind address: every interface, for Java parity. See the module documentation.
pub const DEFAULT_BIND_ADDRESS: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

/// `argv` with `--bind-address` removed, plus the address it named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindAddressSelection {
    /// The remaining arguments, to be handed to `capture-config` unchanged.
    pub remaining_args: Vec<String>,
    /// The address the listener will bind.
    pub bind_address: IpAddr,
    /// True when the value came from the flag or the environment rather than the default.
    pub explicit: bool,
}

/// Why a bind address could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindAddressError {
    /// The flag was given with no value.
    #[error("{BIND_ADDRESS_FLAG} requires an IP address")]
    MissingValue,
    /// The value is not an IP address.
    #[error(
        "{BIND_ADDRESS_FLAG} must be an IP address, got {value:?}; \
         use 0.0.0.0 for every interface or 127.0.0.1 for loopback only"
    )]
    Malformed {
        /// The rejected text.
        value: String,
    },
    /// The environment variable's value is not an IP address.
    #[error("the value of ${BIND_ADDRESS_ENV} must be an IP address, got {value:?}")]
    MalformedEnvironment {
        /// The rejected text.
        value: String,
    },
}

/// Splits `--bind-address` out of `args`, falling back to the environment and then the default.
///
/// `args` must exclude the program name. Both `--bind-address 10.0.0.1` and
/// `--bind-address=10.0.0.1` are accepted; a later occurrence wins, matching how `clap` treats a
/// repeated single-valued argument.
///
/// # Errors
///
/// [`BindAddressError`] when the flag or the environment variable carries something that is not an
/// IP address. Everything else is left in [`BindAddressSelection::remaining_args`] for
/// `capture-config` to accept or reject.
pub fn select_bind_address<S: AsRef<str>>(
    args: &[S],
    environment: &dyn capture_config::EnvLookup,
) -> Result<BindAddressSelection, BindAddressError> {
    let mut remaining_args = Vec::with_capacity(args.len());
    let mut from_flag = None;
    let mut arguments = args.iter().map(AsRef::as_ref);

    while let Some(argument) = arguments.next() {
        if argument == BIND_ADDRESS_FLAG {
            let value = arguments.next().ok_or(BindAddressError::MissingValue)?;
            from_flag = Some(parse(value)?);
        } else if let Some(value) = argument
            .strip_prefix(BIND_ADDRESS_FLAG)
            .and_then(|rest| rest.strip_prefix('='))
        {
            from_flag = Some(parse(value)?);
        } else {
            remaining_args.push(argument.to_owned());
        }
    }

    let (bind_address, explicit) = match from_flag {
        Some(address) => (address, true),
        None => match environment.get(BIND_ADDRESS_ENV).filter(|v| !v.is_empty()) {
            Some(value) => (
                value
                    .parse::<IpAddr>()
                    .map_err(|_| BindAddressError::MalformedEnvironment {
                        value: value.clone(),
                    })?,
                true,
            ),
            None => (DEFAULT_BIND_ADDRESS, false),
        },
    };

    Ok(BindAddressSelection {
        remaining_args,
        bind_address,
        explicit,
    })
}

fn parse(value: &str) -> Result<IpAddr, BindAddressError> {
    value
        .parse::<IpAddr>()
        .map_err(|_| BindAddressError::Malformed {
            value: value.to_owned(),
        })
}

/// The operator-facing sentence describing what this bind plus this client-auth setting admits.
///
/// Emitted as a start-up WARN unconditionally, not only on the wildcard: an operator reading the
/// log should not have to know the default to know the exposure.
#[must_use]
pub fn describe_exposure(bind_address: IpAddr, require_client_auth: bool) -> String {
    let reach = if bind_address.is_unspecified() {
        "every network interface on this host"
    } else if bind_address.is_loopback() {
        "loopback only"
    } else {
        "one specific interface"
    };
    let gate = if require_client_auth {
        "frontside mutual TLS is required, so a client must present a trusted certificate"
    } else {
        "frontside client authentication is DISABLED, so the only caller gate is network-level \
         reachability: anything that can route to this address and port can proxy traffic through \
         it"
    };
    format!("the capture proxy will listen on {bind_address} ({reach}); {gate}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct StubEnv(HashMap<String, String>);

    impl StubEnv {
        fn with(name: &str, value: &str) -> Self {
            Self(HashMap::from([(name.to_owned(), value.to_owned())]))
        }
    }

    impl capture_config::EnvLookup for StubEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn select(args: &[&str]) -> BindAddressSelection {
        select_bind_address(args, &StubEnv::default()).expect("selection succeeds")
    }

    #[test]
    fn the_default_is_the_wildcard_for_java_parity() {
        let selection = select(&["--listen-port", "9200"]);
        assert_eq!(selection.bind_address, DEFAULT_BIND_ADDRESS);
        assert!(selection.bind_address.is_unspecified());
        assert!(!selection.explicit);
        assert_eq!(selection.remaining_args, ["--listen-port", "9200"]);
    }

    #[test]
    fn the_flag_is_removed_from_the_arguments_config_will_parse() {
        // capture-config would reject an unknown flag, so extraction must be complete.
        for form in [
            vec!["--bind-address", "127.0.0.1", "--no-capture"],
            vec!["--bind-address=127.0.0.1", "--no-capture"],
        ] {
            let selection = select(&form);
            assert_eq!(selection.bind_address, IpAddr::from([127, 0, 0, 1]));
            assert!(selection.explicit);
            assert_eq!(selection.remaining_args, ["--no-capture"]);
        }
    }

    #[test]
    fn a_later_occurrence_wins() {
        let selection = select(&["--bind-address", "0.0.0.0", "--bind-address", "127.0.0.1"]);
        assert_eq!(selection.bind_address, IpAddr::from([127, 0, 0, 1]));
        assert!(selection.remaining_args.is_empty());
    }

    #[test]
    fn an_ipv6_address_is_accepted() {
        let selection = select(&["--bind-address", "::1"]);
        assert!(selection.bind_address.is_loopback());
    }

    #[test]
    fn a_missing_or_malformed_value_is_rejected() {
        assert_eq!(
            select_bind_address(&["--bind-address"], &StubEnv::default()),
            Err(BindAddressError::MissingValue)
        );
        assert_eq!(
            select_bind_address(&["--bind-address", "localhost"], &StubEnv::default()),
            Err(BindAddressError::Malformed {
                value: "localhost".to_owned()
            })
        );
    }

    #[test]
    fn the_environment_carries_the_value_the_json_path_cannot() {
        // resolve_args rejects any third argument alongside ---INLINE-JSON, so the environment is
        // the only channel a Kubernetes/Argo deployment has.
        let environment = StubEnv::with(BIND_ADDRESS_ENV, "127.0.0.1");
        let selection = select_bind_address(&["---INLINE-JSON", "e30="], &environment)
            .expect("selection succeeds");
        assert_eq!(selection.bind_address, IpAddr::from([127, 0, 0, 1]));
        assert!(selection.explicit);
        assert_eq!(selection.remaining_args, ["---INLINE-JSON", "e30="]);
    }

    #[test]
    fn the_flag_overrides_the_environment() {
        let environment = StubEnv::with(BIND_ADDRESS_ENV, "10.0.0.1");
        let selection = select_bind_address(&["--bind-address", "127.0.0.1"], &environment)
            .expect("selection succeeds");
        assert_eq!(selection.bind_address, IpAddr::from([127, 0, 0, 1]));
    }

    #[test]
    fn a_malformed_environment_value_is_rejected_rather_than_ignored() {
        let environment = StubEnv::with(BIND_ADDRESS_ENV, "not-an-address");
        assert_eq!(
            select_bind_address::<&str>(&[], &environment),
            Err(BindAddressError::MalformedEnvironment {
                value: "not-an-address".to_owned()
            })
        );
    }

    #[test]
    fn the_exposure_sentence_names_the_address_and_the_absent_caller_gate() {
        let wildcard = describe_exposure(DEFAULT_BIND_ADDRESS, false);
        assert!(wildcard.contains("will listen on 0.0.0.0"), "{wildcard}");
        assert!(wildcard.contains("every network interface"), "{wildcard}");
        assert!(wildcard.contains("DISABLED"), "{wildcard}");
        assert!(wildcard.contains("network-level"), "{wildcard}");

        let gated = describe_exposure(DEFAULT_BIND_ADDRESS, true);
        assert!(gated.contains("mutual TLS is required"), "{gated}");
        assert!(!gated.contains("DISABLED"), "{gated}");

        let loopback = describe_exposure(IpAddr::from([127, 0, 0, 1]), false);
        assert!(loopback.contains("loopback only"), "{loopback}");
    }
}
