#!/usr/bin/env bash
# Memory-growth bisection: same load (plaintext capture, flat out, 24 partitions), 150 s, per image.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs
for tag in 03ddf2ab2 428ef064c e84c1750c 87aa0fc89 c4d825c05; do
  RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-$tag KAFKA_PARTITIONS=24 \
    "$H/run-profile.sh" leak-$tag rust plaintext-capture 150 15 > $L/leak-$tag.log 2>&1
  echo "leak $tag rc=$?"
done
echo LEAK_DONE
