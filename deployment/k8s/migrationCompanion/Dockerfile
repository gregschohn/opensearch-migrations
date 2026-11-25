ARG BASE_IMAGE=docker.io/library/amazonlinux:2023

FROM ${BASE_IMAGE}

# Upgrade to the pinned AL2023 release; the value comes only from gradle.properties (al2023Releasever)
ARG AL2023_RELEASEVER
RUN test -n "${AL2023_RELEASEVER}" || { echo "AL2023_RELEASEVER build arg is required; Gradle passes it from gradle.properties (al2023Releasever)" >&2; exit 1; } && \
    dnf upgrade -y --releasever=${AL2023_RELEASEVER} && dnf clean all && rm -rf /var/cache/dnf

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
