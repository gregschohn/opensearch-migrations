//! Cross-language proof that Rust routes connections to the same partitions as Java.
//!
//! Covers traceability row KAFKA-06's hash component. The fixture was produced by the real
//! kafka-clients 4.2.0 `Utils.murmur2`/`Utils.toPositive`, so a pass here is Java agreement, not
//! self-consistency.

use capture_kafka::routing_hash::{murmur2, select_partition, to_positive};
use capture_test_support::routing_vectors::routing_goldens;

#[test]
fn murmur2_matches_java_for_every_golden_vector() {
    let goldens = routing_goldens();
    assert!(
        !goldens.vectors.is_empty(),
        "fixture must contain vectors; an empty fixture would make this test vacuous"
    );

    let mut checked = 0_usize;
    for vector in &goldens.vectors {
        let bytes = vector.utf8_bytes();
        assert_eq!(
            bytes,
            vector.connection_id.as_bytes(),
            "fixture utf8Hex disagrees with its own connectionId for {:?}",
            vector.connection_id
        );
        assert_eq!(
            murmur2(&bytes),
            vector.murmur2,
            "murmur2 mismatch for {:?} (hex {})",
            vector.connection_id,
            vector.utf8_hex
        );
        assert_eq!(
            to_positive(vector.murmur2),
            vector.to_positive,
            "toPositive mismatch for {:?}",
            vector.connection_id
        );
        checked += 1;
    }
    assert_eq!(checked, goldens.vectors.len());
}

#[test]
fn partition_selection_matches_java_for_every_golden_route() {
    let goldens = routing_goldens();
    let mut checked = 0_usize;
    for vector in &goldens.vectors {
        assert!(
            !vector.routes.is_empty(),
            "every vector must carry routing cases"
        );
        for case in &vector.routes {
            assert_eq!(
                select_partition(&vector.connection_id, &case.partitions),
                case.selected,
                "partition mismatch for {:?} over {:?}",
                vector.connection_id,
                case.partitions
            );
            checked += 1;
        }
    }
    // The committed fixture carries 68 ids x 8 partition sets.
    assert_eq!(
        checked, 544,
        "expected 544 Java routing decisions; the fixture changed without this test being updated"
    );
}

#[test]
fn fixture_documents_its_java_provenance() {
    let goldens = routing_goldens();
    assert_eq!(goldens.kafka_clients, "4.2.0");
    assert!(
        goldens.generator.contains("murmur2"),
        "fixture must record the Java expression it came from"
    );
}
