package org.opensearch.migrations.utils;

import java.util.Optional;
import java.util.UUID;

import lombok.experimental.UtilityClass;
import lombok.extern.slf4j.Slf4j;

@Slf4j
@UtilityClass
public class ProcessHelpers {
    private static final String DEFAULT_NODE_ID = UUID.randomUUID().toString();

    public static String getNodeInstanceName() {
        var nodeId = Optional.of("ECS_TASK_ID").map(System::getenv)
            .or(() -> Optional.of("HOSTNAME").map(System::getenv))
            .orElse(DEFAULT_NODE_ID);
        log.atInfo().setMessage(() -> "getNodeInstanceName()=" + nodeId).log();
        return nodeId;
    }
}
