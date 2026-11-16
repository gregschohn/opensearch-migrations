//! Java-generated golden vectors for connection-to-partition routing.
//!
//! Produced by `qualification/java-fixtures/GenVectors.java` against kafka-clients 4.2.0, which is
//! the exact client the Java capture proxy uses. Regenerate with:
//!
//! ```sh
//! KC=$(find ~/.gradle/caches -name 'kafka-clients-4.2.0.jar' | grep -v sources | head -1)
//! SL=$(find ~/.gradle/caches -name 'slf4j-api-*.jar' | grep -v sources | head -1)
//! javac -cp "$KC" -d /tmp/gv qualification/java-fixtures/GenVectors.java
//! java -cp "$KC:$SL:/tmp/gv" GenVectors \
//!   > crates/capture-test-support/fixtures/murmur2-golden.json
//! ```

use serde::Deserialize;

/// Raw fixture bytes, embedded so tests do not depend on the working directory.
pub const MURMUR2_GOLDEN_JSON: &str = include_str!("../fixtures/murmur2-golden.json");

/// The whole golden-vector document.
#[derive(Debug, Deserialize)]
pub struct RoutingGoldens {
    /// Human-readable description of the Java expression that produced these values.
    pub generator: String,
    /// The kafka-clients version used.
    #[serde(rename = "kafkaClients")]
    pub kafka_clients: String,
    /// One entry per connection id.
    pub vectors: Vec<RoutingVector>,
}

/// One connection id and every routing decision Java made for it.
#[derive(Debug, Deserialize)]
pub struct RoutingVector {
    /// The connection id as a Rust string.
    #[serde(rename = "connectionId")]
    pub connection_id: String,
    /// Lowercase hex of the id's UTF-8 bytes, so the encoding step is pinned independently of any
    /// JSON escaping.
    #[serde(rename = "utf8Hex")]
    pub utf8_hex: String,
    /// `Utils.murmur2` of the UTF-8 bytes, as a signed 32-bit value.
    pub murmur2: i32,
    /// `Utils.toPositive` of `murmur2`.
    #[serde(rename = "toPositive")]
    pub to_positive: i32,
    /// Routing results across several assigned-partition sets.
    pub routes: Vec<RoutingCase>,
}

impl RoutingVector {
    /// The id's UTF-8 bytes, decoded from [`Self::utf8_hex`].
    ///
    /// # Panics
    ///
    /// Panics if the fixture's hex field is malformed, which would mean the committed fixture is
    /// corrupt.
    #[must_use]
    pub fn utf8_bytes(&self) -> Vec<u8> {
        assert!(
            self.utf8_hex.len() % 2 == 0,
            "fixture utf8Hex must have an even length"
        );
        (0..self.utf8_hex.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&self.utf8_hex[i..i + 2], 16)
                    .expect("fixture utf8Hex must be valid hex")
            })
            .collect()
    }
}

/// One assigned-partition set and the partition Java selected from it.
#[derive(Debug, Deserialize)]
pub struct RoutingCase {
    /// The assigned partitions, already unique and ascending as Java's `TreeSet` produces them.
    pub partitions: Vec<i32>,
    /// The partition Java routed the connection to.
    pub selected: i32,
}

/// Parses the embedded golden vectors.
///
/// # Panics
///
/// Panics if the committed fixture does not parse, which is a corrupt-fixture bug rather than a
/// runtime condition.
#[must_use]
pub fn routing_goldens() -> RoutingGoldens {
    serde_json::from_str(MURMUR2_GOLDEN_JSON).expect("committed murmur2 fixture parses")
}
