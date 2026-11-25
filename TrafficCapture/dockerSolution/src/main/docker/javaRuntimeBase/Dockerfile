# Base for the Java runtime images. Jib can't run commands, so the OS update happens here.
# The minimal image plus Corretto skips python3, dnf and other packages unused at runtime.
# The image is pinned here; BASE_REGISTRY only changes where it's pulled from (e.g. a cache).
ARG BASE_REGISTRY=public.ecr.aws/amazonlinux
ARG BASE_IMAGE=${BASE_REGISTRY}/amazonlinux:2023-minimal@sha256:75a2d4226d9b98c914d7eed878a03bef4f365ad548be5d28d464ccba504cec79
FROM ${BASE_IMAGE}

ENV LANG=C.UTF-8 \
    JAVA_HOME=/usr/lib/jvm/java-21-amazon-corretto

# To bump: set AL2023_RELEASEVER to the newest AL2023 release and the BASE_IMAGE digest to the
# image's multi-arch index at that release (docker buildx imagetools inspect <image>).
# src.zip (JDK sources, ~50MB) isn't needed at runtime; the amazoncorretto image drops it too.
ARG AL2023_RELEASEVER=2023.12.20261120
RUN microdnf upgrade -y --releasever=${AL2023_RELEASEVER} --setopt=install_weak_deps=0 --nodocs && \
    microdnf install -y --releasever=${AL2023_RELEASEVER} --setopt=install_weak_deps=0 --nodocs \
        java-21-amazon-corretto-headless && \
    microdnf clean all && \
    rm -rf /var/cache/dnf "${JAVA_HOME}/lib/src.zip"
