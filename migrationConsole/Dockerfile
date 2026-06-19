ARG BASE_IMAGE=migrations/elasticsearch_client_test_console:latest
ARG TOOLS_IMAGE=docker.io/library/amazonlinux:2023
ARG USE_SEEDED_PYTHON_VENV=true

# Stage 1: Download and install tools (kafka, kubectl, helm)
FROM ${TOOLS_IMAGE} AS migration-console-tools

RUN set -eu; \
    dnf install -y \
        --setopt=install_weak_deps=False \
        --setopt=tsflags=nodocs \
        --setopt=keepcache=False \
        --setopt=plugins=0 \
        --setopt=logfilelevel=0 \
        curl-minimal \
        gzip \
        tar && \
    dnf clean all

RUN ARCH=$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/') && \
    if [ "$ARCH" != "amd64" ] && [ "$ARCH" != "arm64" ]; then \
        echo "Unsupported architecture: $ARCH" && exit 1; \
    fi && \
    echo "Using architecture: $ARCH" && \
    echo "$ARCH" > /arch.env

ARG HELM_VERSION="3.14.0"
ARG KUBECTL_VERSION="1.32.1"
ARG KAFKA_VERSION="3.9.2"
ARG STERN_VERSION="1.33.1"

# Get kafka distribution and unpack to 'kafka'
RUN set -eu; \
    mkdir -p /root/.cache/migration-console-downloads /root/kafka-tools/kafka && \
    kafka_archive="kafka_2.13-${KAFKA_VERSION}.tgz" && \
    kafka_tgz="/root/.cache/migration-console-downloads/${kafka_archive}" && \
    if [ ! -x /root/kafka-tools/kafka/bin/kafka-topics.sh ]; then \
      curl -fSL "https://archive.apache.org/dist/kafka/${KAFKA_VERSION}/${kafka_archive}" -o "$kafka_tgz" && \
      tar -tzf "$kafka_tgz" >/dev/null && \
      tar -xzf "$kafka_tgz" -C /root/kafka-tools/kafka --strip-components=1; \
    fi && \
    mkdir -p /root/kafka-tools/aws && \
    msk_jar="/root/.cache/migration-console-downloads/aws-msk-iam-auth-2.3.4-all.jar" && \
    if [ ! -s /root/kafka-tools/kafka/libs/msk-iam-auth.jar ] && [ ! -s "$msk_jar" ]; then \
      curl -fSL "https://github.com/aws/aws-msk-iam-auth/releases/download/v2.3.4/aws-msk-iam-auth-2.3.4-all.jar" -o "$msk_jar"; \
    fi && \
    if [ ! -s /root/kafka-tools/kafka/libs/msk-iam-auth.jar ]; then \
      cp "$msk_jar" /root/kafka-tools/kafka/libs/msk-iam-auth.jar; \
    fi

# Get kubectl and Helm distributions
RUN ARCH=$(cat /arch.env) && \
    mkdir -p /k8s/bin && \
    if command -v kubectl >/dev/null 2>&1; then \
        cp "$(command -v kubectl)" /k8s/bin/kubectl; \
    else \
        curl -fSL "https://dl.k8s.io/release/v${KUBECTL_VERSION}/bin/linux/${ARCH}/kubectl" -o /k8s/bin/kubectl; \
    fi && \
    chmod +x /k8s/bin/kubectl

RUN ARCH=$(cat /arch.env) && \
    mkdir -p /k8s/bin && \
    if command -v helm >/dev/null 2>&1; then \
        cp "$(command -v helm)" /k8s/bin/helm; \
    else \
        curl -fSL "https://get.helm.sh/helm-v${HELM_VERSION}-linux-${ARCH}.tar.gz" | tar -xz && \
        mv linux-${ARCH}/helm /k8s/bin/; \
    fi

# Install stern - used by 'workflow log -f' for multi-pod log tailing
RUN ARCH=$(cat /arch.env) && \
    mkdir -p /k8s/bin && \
    if command -v stern >/dev/null 2>&1; then \
        cp "$(command -v stern)" /k8s/bin/stern; \
    else \
        curl -fSL \
            "https://github.com/stern/stern/releases/download/v${STERN_VERSION}/stern_${STERN_VERSION}_linux_${ARCH}.tar.gz" | tar -xz && \
        mv stern /k8s/bin/; \
    fi && \
    chmod +x /k8s/bin/stern

RUN set -eu; \
    mkdir -p /seeded-tools/rootfs; \
    copy_seeded_path() { \
        path="$1"; \
        if [ -e "$path" ]; then \
            mkdir -p "/seeded-tools/rootfs$(dirname "$path")"; \
            cp -a "$path" "/seeded-tools/rootfs${path}"; \
        fi; \
    }; \
    for path in \
        /etc/bash_completion.d/redefine_filedir \
        /etc/fuse.conf \
        /etc/npmrc \
        /etc/profile.d/bash_completion.sh \
        /.venv \
        /opt/aws/mountpoint-s3 \
        /usr/bin/fusermount \
        /usr/bin/mount-s3 \
        /usr/bin/node-22 \
        /usr/bin/npm-22 \
        /usr/bin/npx-22 \
        /usr/bin/ulockmgr_server \
        /usr/lib/nodejs22 \
        /usr/lib64/libnode.so.127 \
        /usr/lib64/libv8.so.12.4 \
        /usr/lib64/libv8_libbase.so.12.4 \
        /usr/lib64/libv8_libplatform.so.12.4 \
        /usr/sbin/mount.mount-s3 \
        /usr/share/bash-completion; do \
        copy_seeded_path "$path"; \
    done; \
    for path in /usr/lib64/libfuse.so.2* /usr/lib64/libulockmgr.so.1*; do \
        copy_seeded_path "$path"; \
    done


# Stage 2: Main migration console base image with Python environment and scripts
FROM ${BASE_IMAGE}
ARG USE_SEEDED_PYTHON_VENV=true

# Define the virtual environment path
ENV WORKON_HOME=/
ENV PIPENV_CUSTOM_VENV_NAME=.venv
ENV PIPENV_DEFAULT_PYTHON_VERSION=3.11
ENV PIPENV_MAX_DEPTH=1
ENV PATH="/.venv/bin:${PATH}"

# Copy tools from previous stage
COPY --from=migration-console-tools /root/kafka-tools /root/kafka-tools
COPY --from=migration-console-tools /k8s/bin/ /usr/local/bin
COPY --from=migration-console-tools /seeded-tools/rootfs/ /

RUN kubectl completion bash > /etc/bash_completion.d/kubectl && \
    helm completion bash > /etc/bash_completion.d/helm && \
    mkdir -p /config

WORKDIR /root

# Install Node.js for config processor and workflow template creation
# NPM is to setup the schema checking that coc uses
RUN set -eu; \
    if [ -x /usr/bin/node-22 ] && \
       [ -e /usr/lib64/libnode.so.127 ] && \
       [ -e /usr/share/bash-completion/bash_completion ] && \
       [ -e /usr/lib64/libfuse.so.2 ]; then \
        ln -sf /usr/bin/node-22 /usr/bin/node; \
        ln -sf ../lib/nodejs22/lib/node_modules/npm/bin/npm-cli.js /usr/bin/npm-22; \
        ln -sf ../lib/nodejs22/lib/node_modules/npm/bin/npx-cli.js /usr/bin/npx-22; \
        ln -sf /usr/bin/npm-22 /usr/bin/npm; \
        ln -sf /usr/bin/npx-22 /usr/bin/npx; \
    else \
        dnf install -y  \
            --setopt=install_weak_deps=False \
            --setopt=tsflags=nodocs \
            --setopt=keepcache=False \
            --setopt=plugins=0 \
            --setopt=logfilelevel=0 \
            nodejs22 \
            nodejs22-npm \
            bash-completion \
            fuse \
            fuse-libs; \
        dnf clean all; \
    fi

# Install mount-s3 for direct S3 bucket mounting (requires SYS_ADMIN capability at runtime)
RUN MOUNT_S3_VERSION="1.22.2" && \
    ARCH=$(uname -m) && \
    if [ "$ARCH" = "aarch64" ]; then ARCH="arm64"; fi && \
    if ! command -v mount-s3 >/dev/null 2>&1; then \
        mkdir -p /root/.cache/migration-console-downloads && \
        mount_s3_rpm="/root/.cache/migration-console-downloads/mount-s3-${MOUNT_S3_VERSION}-${ARCH}.rpm" && \
        curl -fL \
            "https://s3.amazonaws.com/mountpoint-s3-release/${MOUNT_S3_VERSION}/${ARCH}/mount-s3-${MOUNT_S3_VERSION}-${ARCH}.rpm" \
            -o "${mount_s3_rpm}" && \
        rpm -Uvh "${mount_s3_rpm}"; \
    fi && \
    chmod +x "$(command -v mount-s3)"

COPY setupVimSchemaChecker.sh /tmp/setupVimSchemaChecker.sh
RUN chmod +x /tmp/setupVimSchemaChecker.sh && \
    /tmp/setupVimSchemaChecker.sh && \
    rm /tmp/setupVimSchemaChecker.sh

# Copy migration console scripts and configs
COPY msk-iam-auth.properties /root/kafka-tools/aws
COPY kafkaCmdRef.md /root/kafka-tools
COPY kafkaExport.sh /root/kafka-tools
COPY kafkaImport.sh /root/kafka-tools
RUN chmod ug+x /root/kafka-tools/kafkaExport.sh /root/kafka-tools/kafkaImport.sh

COPY loadServicesFromParameterStore.sh /root/
RUN chmod ug+x /root/loadServicesFromParameterStore.sh

COPY start-console.sh /root/
RUN chmod +x /root/start-console.sh

# Install Python dependencies before copying full source trees so normal Python
# code edits do not invalidate the network-heavy pipenv layers.
WORKDIR /root
RUN mkdir -p /root/lib/console_link/console_link \
    /root/lib/integ_test \
    /root/cluster_tools/src/cluster_tools

COPY lib/console_link/Pipfile lib/console_link/Pipfile.lock lib/console_link/setup.py /root/lib/console_link/
COPY lib/console_link/console_link/__init__.py /root/lib/console_link/console_link/
WORKDIR /root/lib/console_link
RUN if [ "${USE_SEEDED_PYTHON_VENV}" = "true" ] && [ -x /.venv/bin/python ]; then \
        echo "Using seeded Python virtualenv for console_link dependencies"; \
    else \
        pipenv install --deploy; \
    fi

COPY lib/integ_test/Pipfile lib/integ_test/Pipfile.lock /root/lib/integ_test/
WORKDIR /root/lib/integ_test
RUN if [ "${USE_SEEDED_PYTHON_VENV}" = "true" ] && [ -x /.venv/bin/python ]; then \
        echo "Using seeded Python virtualenv for integ_test dependencies"; \
    else \
        pipenv install --deploy; \
    fi

COPY cluster_tools/Pipfile cluster_tools/Pipfile.lock cluster_tools/setup.py /root/cluster_tools/
COPY cluster_tools/src/cluster_tools/__init__.py /root/cluster_tools/src/cluster_tools/
WORKDIR /root/cluster_tools
RUN if [ "${USE_SEEDED_PYTHON_VENV}" = "true" ] && [ -x /.venv/bin/python ]; then \
        echo "Using seeded Python virtualenv for cluster_tools dependencies"; \
    else \
        pipenv install; \
    fi

COPY lib/console_link/console_link /root/lib/console_link/console_link
COPY lib/integ_test /root/lib/integ_test
COPY cluster_tools /root/cluster_tools
RUN rm -rf /root/cluster_tools/.venv
WORKDIR /root

RUN mkdir -p /usr/share/bash-completion/completions && \
    _CONSOLE_COMPLETE=bash_source console > /usr/share/bash-completion/completions/console && \
    test -s /usr/share/bash-completion/completions/console

COPY nodeStaging/configProcessor /root/configProcessor

RUN chmod +x /root/configProcessor/*.sh
ENV CONFIG_PROCESSOR_DIR=/root/configProcessor
ENV NODEJS=node-22

COPY nodeStaging/migrationResources /root/migrationResources
ENV MIGRATION_RESOURCES_DIR=/root/migrationResources

COPY nodeStaging/workflowTemplates /root/workflows
RUN chmod +x /root/workflows/.workflowScripts/*.sh
COPY workflowConfigToServicesConfig.jq /root/
ENV WORKFLOW_CONFIG_JQ_SCRIPT=/root/workflowConfigToServicesConfig.jq

# Console setup bash completion and venv for interactive access
RUN echo '. /.venv/bin/activate' >> /etc/profile.d/venv.sh && \
    echo '. /etc/profile.d/bash_completion.sh' >> ~/.bashrc && \
    echo '. /etc/profile.d/venv.sh' >> ~/.bashrc && \
    echo 'echo Welcome to the Migration Assistant Console' >> ~/.bashrc && \
    echo 'eval "$(register-python-argcomplete cluster_tools)"' >> ~/.bashrc && \
    echo '. /usr/share/bash-completion/completions/console' >> ~/.bashrc && \
    echo 'workflow util completions bash > /usr/share/bash-completion/completions/workflow' >> ~/.bashrc && \
    echo '. /usr/share/bash-completion/completions/workflow' >> ~/.bashrc && \
    echo 'PS1="(\\t) \\[\\e[92m\\]migration-console \\[\\e[0m\\](\\w) -> "'>> ~/.bashrc

# Set ENV to control startup script in /bin/sh mode
ENV ENV=/root/.bashrc

CMD ["/root/start-console.sh"]
