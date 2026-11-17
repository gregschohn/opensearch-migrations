package org.opensearch.migrations.testutils;

import org.testcontainers.utility.DockerImageName;

public interface SharedDockerImageNames {
    DockerImageName KAFKA = DockerImageName.parse("confluentinc/cp-kafka:7.5.0");
    DockerImageName HTTPD = DockerImageName.parse("httpd:2.4.66-alpine");

    /**
     * The Rust capture proxy image built from {@code TrafficCapture/captureProxy/Dockerfile}.
     *
     * <p>No public registry has it, so it must exist before a test that uses it runs. The reference comes from
     * the {@code captureProxyImage} system property, which Gradle sets from {@code -PcaptureProxyImage=<ref>};
     * the default is the tag a plain local build would use:
     * {@code docker build --file TrafficCapture/captureProxy/Dockerfile --tag migrations/capture_proxy:latest
     * TrafficCapture}.
     */
    DockerImageName CAPTURE_PROXY = DockerImageName.parse(
        System.getProperty("captureProxyImage", "migrations/capture_proxy:latest")
    );
}
