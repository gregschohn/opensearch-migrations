# Base for the Java runtime images. Jib can't run commands, so the OS update happens here.
ARG BASE_IMAGE=docker.io/library/amazoncorretto:21-al2023-headless
FROM ${BASE_IMAGE}

# Upgrade to the pinned AL2023 release; the value comes only from gradle.properties (al2023Releasever)
ARG AL2023_RELEASEVER
RUN test -n "${AL2023_RELEASEVER}" || { echo "AL2023_RELEASEVER build arg is required; Gradle passes it from gradle.properties (al2023Releasever)" >&2; exit 1; } && \
    dnf upgrade -y --releasever=${AL2023_RELEASEVER} && dnf clean all && rm -rf /var/cache/dnf
