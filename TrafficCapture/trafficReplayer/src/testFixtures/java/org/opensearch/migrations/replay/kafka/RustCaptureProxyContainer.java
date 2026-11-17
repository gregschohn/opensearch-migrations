/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * The OpenSearch Contributors require contributions made to
 * this file be licensed under the Apache-2.0 license or a
 * compatible open source license.
 */

package org.opensearch.migrations.replay.kafka;

import java.net.URI;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;

import lombok.extern.slf4j.Slf4j;
import org.testcontainers.containers.GenericContainer;
import org.testcontainers.containers.Network;
import org.testcontainers.containers.output.Slf4jLogConsumer;
import org.testcontainers.containers.wait.strategy.Wait;
import org.testcontainers.utility.DockerImageName;

/**
 * The Rust capture proxy ({@code TrafficCapture/captureProxy}), run from its {@code capture_proxy} image.
 *
 * <p>Unlike the Java fixture it replaces, this is a real container: the proxy under test is the shipped
 * binary in the shipped image, not a class on the test's classpath. Kafka and the destination therefore
 * have to be addressed as the container sees them, which is why the constructor takes a {@link Network}
 * and network-side URIs rather than the {@code localhost:mappedPort} forms the host uses.
 *
 * <p>Readiness is the listen port. The proxy binds it only after its Kafka capability probe has succeeded
 * (or immediately with {@code --no-capture}), so a listening port means captured traffic can flow. No
 * request is sent to check readiness, so the topic holds only what a test drives through
 * {@link #endpoint()}.
 *
 * <p>The image is a prerequisite, not something this fixture builds or pulls: see
 * {@code SharedDockerImageNames.CAPTURE_PROXY}.
 */
@Slf4j
public class RustCaptureProxyContainer extends GenericContainer<RustCaptureProxyContainer> {
    /** The port the proxy listens on inside its container; the host reaches it through the mapped port. */
    public static final int LISTEN_PORT = 9200;
    private static final Duration STARTUP_TIMEOUT = Duration.ofSeconds(60);

    private final DockerImageName image;

    /**
     * @param kafkaBrokers {@code host:port} as reachable from inside {@code network}, or {@code null} to run
     *                     with {@code --no-capture}
     * @param destinationUri the source cluster as reachable from inside the container
     * @param extraArgs further proxy flags, appended after the connection flags
     */
    public RustCaptureProxyContainer(
        DockerImageName image,
        Network network,
        String kafkaBrokers,
        String destinationUri,
        List<String> extraArgs
    ) {
        super(image);
        this.image = image;
        var args = new ArrayList<String>();
        if (kafkaBrokers != null) {
            args.add("--kafka-brokers");
            args.add(kafkaBrokers);
        } else {
            args.add("--no-capture");
        }
        args.add("--destination-uri");
        args.add(destinationUri);
        args.add("--listen-port");
        args.add(String.valueOf(LISTEN_PORT));
        args.add("--insecure-destination");
        args.addAll(extraArgs);

        withNetwork(network);
        // The destination is a host process; ProxyWrittenTopic exposes its port with
        // Testcontainers.exposeHostPorts, which this container reaches as host.testcontainers.internal.
        withAccessToHost(true);
        withExposedPorts(LISTEN_PORT);
        // The image's entrypoint is the proxy binary, so the command is exactly its arguments.
        withCommand(args.toArray(new String[0]));
        withLogConsumer(new Slf4jLogConsumer(log).withPrefix("capture-proxy"));
        waitingFor(Wait.forListeningPort().withStartupTimeout(STARTUP_TIMEOUT));
    }

    @Override
    public void start() {
        try {
            super.start();
        } catch (RuntimeException startupFailure) {
            throw new IllegalStateException(
                "The capture proxy container ("
                    + image.asCanonicalNameString()
                    + ") did not start; its log is above and the cause is attached. If the image is missing,"
                    + " build it with `docker build --file TrafficCapture/captureProxy/Dockerfile --tag <ref>"
                    + " TrafficCapture` and pass -PcaptureProxyImage=<ref> to Gradle.",
                startupFailure
            );
        }
    }

    /** The proxy as the host (the test) reaches it. */
    public URI endpoint() {
        return URI.create("http://" + getHost() + ":" + getMappedPort(LISTEN_PORT));
    }
}
