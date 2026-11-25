# Global ARGs (must be before first FROM to be usable in FROM lines)
# The image is pinned here; BASE_REGISTRY only changes where it's pulled from (e.g. a cache).
# To bump: update this digest (multi-arch index at the release) and AL2023_RELEASEVER below.
ARG BASE_REGISTRY=docker.io/library
ARG BUILDER_IMAGE=${BASE_REGISTRY}/amazonlinux:2023@sha256:edd3297df7d444ab9a7896b763d5305d4bab57d2fb3943e831c1713b97d32d9d
ARG RUNTIME_IMAGE=${BUILDER_IMAGE}

# ---------- 1) Builder ----------
FROM ${BUILDER_IMAGE} AS builder

# Build deps (toolchain + headers)
RUN dnf install -y \
      --setopt=install_weak_deps=False \
      --setopt=tsflags=nodocs \
      --setopt=keepcache=False \
      --setopt=plugins=0 \
      --setopt=logfilelevel=0 \
      gcc \
      libffi-devel \
      make \
      openssl-devel \
      pkg-config \
      python3.11 \
      python3.11-devel \
      python3.11-pip \
      python3.11-wheel \
  && dnf clean all

ENV PIP_ROOT_USER_ACTION=ignore
ENV LANG=C.UTF-8

# Define the virtual environment path to use for all pipenv runs
ENV WORKON_HOME=/
ENV PIPENV_CUSTOM_VENV_NAME=.venv
ENV PIPENV_DEFAULT_PYTHON_VERSION=3.11
ENV PIPENV_MAX_DEPTH=1

# Pipenv & project install into a project-local venv we can copy out
RUN python3.11 -m pip install pipenv==2026.5.0
WORKDIR /
RUN python3.11 -m venv .venv
COPY Pipfile Pipfile.lock ./
RUN pipenv install --deploy

# ---------- 2) Runtime ----------
FROM ${RUNTIME_IMAGE}

ENV PIP_ROOT_USER_ACTION=ignore
ENV LANG=C.UTF-8

# Upgrade to the pinned AL2023 release (later installs here and in the console images use it too).
# Bump it with the base image digest above.
ARG AL2023_RELEASEVER=2023.12.20261120
RUN dnf upgrade -y --releasever=${AL2023_RELEASEVER} && dnf clean all && rm -rf /var/cache/dnf

# Minimal runtime deps (no compilers, no *-devel)
RUN dnf install -y \
      --setopt=install_weak_deps=False \
      --setopt=tsflags=nodocs \
      --setopt=keepcache=False \
      --setopt=plugins=0 \
      --setopt=logfilelevel=0 \
        curl-minimal \
        diffutils \
        git \
        gzip \
        hostname \
        java-21-amazon-corretto-headless \
        jq \
        less \
        make \
        openssl \
        pkg-config \
        procps-ng \
        python3.11 \
        python3.11-pip \
        tar \
        unzip \
        vim \
        which \
        wget \
        zlib \
        && \
    dnf clean all

# Define the virtual environment path to use for all pipenv runs
ENV WORKON_HOME=/
ENV PIPENV_CUSTOM_VENV_NAME=.venv
ENV PIPENV_DEFAULT_PYTHON_VERSION=3.11
ENV PIPENV_MAX_DEPTH=1
ENV PATH="/.venv/bin:${PATH}"

RUN python3.11 -m pip install pipenv==2026.5.0
WORKDIR /
COPY --from=builder /.venv /.venv

# Copy the app and the built virtualenv from builder
WORKDIR /root
COPY Pipfile .
COPY Pipfile.lock .
# Already built from builder image, no build necessary

COPY *.sh /root/
COPY *.py /root/

RUN chmod ug+x /root/*.sh
RUN chmod ug+x /root/*.py
RUN ln -s $(command -v vim) /usr/local/bin/vi

CMD ["tail", "-f", "/dev/null"]
