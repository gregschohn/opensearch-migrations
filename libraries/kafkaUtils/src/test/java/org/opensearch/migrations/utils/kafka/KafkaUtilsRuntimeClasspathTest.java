package org.opensearch.migrations.utils.kafka;

import java.io.File;
import java.net.URLClassLoader;
import java.util.Arrays;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

/**
 * The shipped runtime classpath carries the MSK IAM login module.
 *
 * <p>{@link KafkaConfig} names {@code software.amazon.msk.auth.iam.*} only as strings, so dropping the
 * {@code aws-msk-iam-auth} runtime dependency compiles cleanly and fails only when the migration console
 * imports into MSK, with {@code ClassNotFoundException: IAMLoginModule}.
 */
class KafkaUtilsRuntimeClasspathTest {
    @Test
    void shippingRuntimeClasspathCarriesTheMskIamClasses() throws Exception {
        var classpath = System.getProperty("kafkaUtilsMainRuntimeClasspath");
        Assertions.assertNotNull(classpath, "the build must pass kafkaUtilsMainRuntimeClasspath");
        var urls = Arrays.stream(classpath.split(File.pathSeparator))
            .map(File::new)
            .map(file -> {
                try {
                    return file.toURI().toURL();
                } catch (Exception e) {
                    throw new IllegalArgumentException(e);
                }
            })
            .toArray(java.net.URL[]::new);

        try (var loader = new URLClassLoader(urls, ClassLoader.getPlatformClassLoader())) {
            Assertions.assertDoesNotThrow(
                () -> Class.forName("software.amazon.msk.auth.iam.IAMLoginModule", false, loader)
            );
            Assertions.assertDoesNotThrow(
                () -> Class.forName("software.amazon.msk.auth.iam.IAMClientCallbackHandler", false, loader)
            );
            Assertions.assertDoesNotThrow(
                () -> Class.forName(KafkaLoaderFromFile.class.getName(), true, loader)
            );
        }
    }
}
