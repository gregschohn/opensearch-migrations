# Capture proxy container image.
#
# Retains the existing capture-proxy product/repository identity: this image replaces the Java
# assembly under the same name, so users and orchestration do not acquire a language-specific one.
#
# # Why AL2023 on both stages
#
# librdkafka is linked against the distribution's SASL and TLS libraries. A previous attempt at this
# rewrite built on a Debian builder and ran on AL2023, which produced an image that compiled
# cleanly and then failed at process start because the binary wanted `libsasl2.so.2` while the
# AL2023 runtime provides a different SONAME. Both stages are therefore AL2023, and the runtime
# installs the matching SASL plugin packages rather than relying on what the builder happened to
# link.
#
# `libcurl-devel` is a *build* requirement even though librdkafka is configured with
# `WITH_CURL=0`: rdkafka-sys still compiles the OIDC configuration surface that
# `WITH_SASL_OAUTHBEARER=1` pulls in, and that includes `curl/curl.h`. Omitting it fails with
# `fatal error: curl/curl.h: No such file or directory` in `rdkafka_conf.c`.

# Both stages must stay AL2023 (see above). The image pipeline (buildImages/build.gradle) passes the
# same image through its registry pull-through cache when one is configured.
ARG BUILDER_IMAGE=public.ecr.aws/amazonlinux/amazonlinux:2023
ARG RUNTIME_IMAGE=public.ecr.aws/amazonlinux/amazonlinux:2023

# ---------------------------------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------------------------------
FROM ${BUILDER_IMAGE} AS builder

# cmake and a C/C++ toolchain are required because rdkafka is built with the `cmake-build` feature,
# which compiles the vendored librdkafka from source rather than trusting a system package.
RUN dnf -y update \
 && dnf -y install \
      gcc \
      gcc-c++ \
      make \
      cmake \
      openssl-devel \
      cyrus-sasl-devel \
      libcurl-devel \
      zlib-devel \
      libzstd-devel \
      patch \
      tar \
      gzip \
      xz \
 && dnf clean all \
 && rm -rf /var/cache/dnf

# Pin the toolchain to the version the workspace declares, so the image build cannot silently drift
# onto a different compiler from the one the tests ran against.
ARG RUST_VERSION=1.97.1
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "${RUST_VERSION}" \
 && rustc --version \
 && cargo --version

WORKDIR /build

# The capture schema lives outside the Cargo workspace and is shared with the Java replayer.
# capture-wire's build script generates from it in place and refuses to vendor a copy, so the build
# context must be TrafficCapture/ rather than TrafficCapture/captureProxy/.
COPY captureProtobufs/src/main/proto/ /build/captureProtobufs/src/main/proto/
COPY captureProxy/ /build/captureProxy/

WORKDIR /build/captureProxy

# `--locked` so the image build fails rather than silently resolving different dependency versions
# than the ones the test suite and the SBOM were produced against.
#
# CARGO_PROFILE=profiling builds the same code with release code generation but keeps symbols, for
# CPU profiling only. Performance gates are measured on the default `release` profile.
ARG CARGO_PROFILE=release
# librdkafka 2.12.1 plus two upstream idempotent-producer fixes, recreated from the checksum-pinned
# crate and the committed patches (third_party/rdkafka-sys/README.md).
RUN third_party/rdkafka-sys/prepare.sh \
 && cargo build --profile "${CARGO_PROFILE}" --locked --bin capture-proxy \
 && mkdir -p /build/out \
 && cp "target/${CARGO_PROFILE}/capture-proxy" /build/out/capture-proxy \
 && if [ "${CARGO_PROFILE}" = "release" ]; then strip /build/out/capture-proxy; fi \
 && /build/out/capture-proxy --version

# ---------------------------------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE}

# The SASL mechanism plugins must be present at runtime: librdkafka loads them dynamically, so a
# missing plugin surfaces as an authentication failure against the broker rather than a link error
# at start. `cyrus-sasl-plain` covers SASL/PLAIN and SASL/OAUTHBEARER's PLAIN-shaped exchange, which
# is the mechanism the MSK IAM path uses; `cyrus-sasl-scram` covers SCRAM-SHA-512.
RUN dnf -y update \
 && dnf -y install \
      openssl-libs \
      cyrus-sasl-lib \
      cyrus-sasl-plain \
      cyrus-sasl-scram \
      zlib \
      libzstd \
      ca-certificates \
 && dnf clean all \
 && rm -rf /var/cache/dnf

# Run unprivileged. The proxy binds a port above 1024 and reads only mounted configuration and
# certificates, so it needs no capability beyond that. shadow-utils provides groupadd/useradd,
# which the AL2023 base image does not ship.
RUN dnf -y install shadow-utils \
 && groupadd --gid 10001 captureproxy \
 && useradd --uid 10001 --gid 10001 --no-create-home --shell /sbin/nologin captureproxy \
 && dnf -y remove shadow-utils \
 && dnf clean all \
 && rm -rf /var/cache/dnf

COPY --from=builder /build/out/capture-proxy /usr/local/bin/capture-proxy

USER 10001:10001

# No CMD: the proxy requires configuration and must refuse to start without it rather than come up
# with defaults. The Kubernetes/Argo path supplies ["---INLINE-JSON", "<base64 JSON>"] as args.
ENTRYPOINT ["/usr/local/bin/capture-proxy"]
