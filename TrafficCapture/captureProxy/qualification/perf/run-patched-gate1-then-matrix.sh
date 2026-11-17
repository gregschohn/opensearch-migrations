#!/usr/bin/env bash
set -u
W=/tmp/rust-proxy-qual/wire-proof; L=/tmp/rust-proxy-qual/logs
cd "$W"
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-428ef064c RUN_ID=wp1
docker ps --format '{{.Names}}' | grep -q '^wpkafka-wp1$' || ./bin/wireproof.sh up > $L/wp1-up-patched.log 2>&1
./bin/wireproof.sh run r4 rust > $L/wp1-r4.log 2>&1; echo "r4 rc=$?"
RUST_PROXY_ARGS="--listenPort 9201 --destinationUri http://wp-origin:9200 --insecureDestination --kafkaBrokers wpkafka-wp1:9092 --kafkaTopic wp-wp1-r5 --kafkaClientId wireproof-wp1-r5 --suppressCaptureForUriPath /_suppressed/.* --traffic-stream-flush-interval-seconds 2 --kafka-producer-count 4" \
  ./bin/wireproof.sh run r5 rust > $L/wp1-r5.log 2>&1; echo "r5 rc=$?"
for t in r4 r5; do
  python3 -I bin/analyze.py --records out/wp1/$t/records.jsonl --manifest out/wp1/$t/driver.json --label $t --out out/wp1/$t/analysis.json > out/wp1/$t/analysis.txt 2>&1; echo "analyze $t rc=$?"
  python3 -I bin/normalize.py --transcript out/wp1/$t/replayer.stdout --out out/wp1/$t/normal.json > /dev/null 2>&1; echo "normalize $t rc=$?"
  python3 -I bin/compare.py --root out/wp1 --a j1 --b $t --out out/wp1/compare-j1-$t.json > out/wp1/compare-j1-$t.txt 2>&1; echo "compare j1-$t rc=$?"
done
echo GATE1_DONE
/tmp/rust-proxy-qual/harness/run-patched-matrix.sh
