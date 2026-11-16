#!/usr/bin/env bash
# DIAGNOSTIC: per-thread CPU inside the Rust proxy during a kafka-delayed run with 4 producers.
set -u
H=/tmp/rust-proxy-qual/harness; L=/tmp/rust-proxy-qual/logs; OUT=/tmp/rust-proxy-qual/results/spin-threads.txt
export RUST_PROXY_IMAGE=localhost:5001/migrations/capture_proxy:rust-cf1eb57ff
PROXY_EXTRA_ARGS="--kafka-producer-count 4" "$H/run-profile.sh" spin-delayed-n4 rust kafka-delayed 30 15 > $L/spin-delayed-n4.log 2>&1 &
RUNPID=$!
until docker ps --format '{{.Names}}' | grep -q '^px-spin-delayed-n4$'; do sleep 1; done
sleep 45
snap() { docker exec px-spin-delayed-n4 sh -c 'for t in /proc/1/task/*; do printf "%s %s\n" "$(cat $t/comm | tr " " _)" "$(cut -d" " -f14,15 $t/stat)"; done'; }
snap > /tmp/spin-a.txt; sleep 5; snap > /tmp/spin-b.txt
{ echo "thread utime+stime ticks over 5 s (100 ticks = 1 core)"; paste /tmp/spin-a.txt /tmp/spin-b.txt | awk '{d=($5+$6)-($2+$3); a[$1]+=d; c[$1]++} END {for (k in a) printf "%6d  %s x%d\n", a[k], k, c[k]}' | sort -rn; } > $OUT
wait $RUNPID; echo "SPIN_DONE rc=$?"
