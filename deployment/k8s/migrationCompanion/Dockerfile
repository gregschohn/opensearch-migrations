# The image is pinned here; BASE_REGISTRY only changes where it's pulled from (e.g. a cache).
# To bump: update this digest (multi-arch index at the release) and AL2023_RELEASEVER below.
ARG BASE_REGISTRY=docker.io/library
ARG BASE_IMAGE=${BASE_REGISTRY}/amazonlinux:2023@sha256:edd3297df7d444ab9a7896b763d5305d4bab57d2fb3943e831c1713b97d32d9d

FROM ${BASE_IMAGE}

ARG AL2023_RELEASEVER=2023.12.20261120
RUN dnf upgrade -y --releasever=${AL2023_RELEASEVER} && dnf clean all && rm -rf /var/cache/dnf

ARG MIGRATION_ASSISTANT_VERSION=0.0.0-dev

LABEL org.opencontainers.image.title="OpenSearch Migrations Migration Companion" \
      org.opencontainers.image.description="Migration-assistant release artifact carrier with the packaged Helm chart" \
      org.opencontainers.image.version="${MIGRATION_ASSISTANT_VERSION}" \
      org.opencontainers.image.licenses="Apache-2.0"

ENV MIGRATION_COMPANION_HOME=/opt/migration-companion \
    MIGRATION_ASSISTANT_HELM_CHART=/opt/migration-companion/artifacts/helm/migration-assistant.tgz

COPY artifacts/ /opt/migration-companion/artifacts/

RUN set -eux; \
    ln -sf /opt/migration-companion/artifacts/helm/migration-assistant.tgz /opt/migration-companion/migration-assistant.tgz

WORKDIR /workspace
