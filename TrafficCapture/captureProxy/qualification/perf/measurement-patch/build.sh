#!/usr/bin/env bash
# Build the MEASUREMENT-ONLY class overlay.
#
# This compiles a patched copy of RootOtelContext against the capture-proxy
# image's own classpath, OUTSIDE the repository. The repository is never
# modified: the source here is a copy, and at run time run-profile.sh mounts
# the resulting classes at /patch and prepends them to the image classpath, so
# class loading picks this RootOtelContext over the one in
# /app/libs/coreUtilities-*.jar.
#
# Why an overlay rather than editing the repo: the working tree is on branch
# rust-capture-proxy with other agents actively committing to it, so an edit to
# a shared Java file would land in someone else's commit. An overlay has
# exactly the same measurement effect with no coordination hazard.
#
# The resulting image overlay is MEASUREMENT-ONLY and must never be used for a
# correctness or compatibility claim.
set -euo pipefail
M="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE="${JAVA_PROXY_IMAGE:-localhost:5001/migrations/capture_proxy:latest_arm64}"
JH="${JAVA_HOME:-$(/usr/libexec/java_home -v 21)}"
LOMBOK="${LOMBOK_JAR:-$(find "$HOME/.gradle/caches" -name 'lombok-1.*.jar' 2>/dev/null | grep -v sources | head -1)}"
[[ -n "$LOMBOK" && -f "$LOMBOK" ]] || { echo "cannot find a lombok jar" >&2; exit 1; }

# Refresh the compile classpath from the image itself, so the overlay is always
# compiled against exactly the bytecode it will run beside.
if [[ ! -d "$M/imagecp/libs" || "${REFRESH_CP:-0}" == 1 ]]; then
  rm -rf "$M/imagecp"; mkdir -p "$M/imagecp"
  CID="$(docker create "$IMAGE")"
  docker cp "$CID":/app/libs "$M/imagecp/libs" >/dev/null
  docker cp "$CID":/app/classes "$M/imagecp/classes" >/dev/null
  docker rm "$CID" >/dev/null
fi

CP="$M/imagecp/classes:$(ls "$M"/imagecp/libs/*.jar | tr '\n' ':')"
rm -rf "$M/classes.staging"; mkdir -p "$M/classes.staging"
"$JH/bin/javac" -proc:full -nowarn -cp "$CP:$LOMBOK" -d "$M/classes.staging" \
  "$M/src/org/opensearch/migrations/tracing/RootOtelContext.java"

# Swap in atomically so a concurrently-starting container never sees a
# half-written class file.
rm -rf "$M/classes.old"
[[ -d "$M/classes" ]] && mv "$M/classes" "$M/classes.old"
mv "$M/classes.staging" "$M/classes"
rm -rf "$M/classes.old"
echo "built: $(find "$M/classes" -name '*.class' | wc -l | tr -d ' ') class file(s)"
