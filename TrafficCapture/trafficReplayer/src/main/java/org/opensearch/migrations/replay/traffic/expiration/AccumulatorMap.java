package org.opensearch.migrations.replay.traffic.expiration;

import org.opensearch.migrations.replay.Accumulation;

import java.util.concurrent.ConcurrentHashMap;

public class AccumulatorMap extends ConcurrentHashMap<ScopedConnectionIdKey, Accumulation> {
}
