{{/*
  Retry Helper — bounded per-step retry for transient failures.

  Defines `retry`, used by install/uninstall/s3 jobs to wrap individual
  network or API calls that occasionally flake (ECR throttling, DNS, helm
  registry resolution, LocalStack startup, transient API server hiccups).

  Per-step retry is preferred over Job-level backoffLimit because it isolates
  the failure to the step that flaked, instead of restarting the whole pod
  and replaying every prior step.
*/}}
{{- define "migration.retryFunctions" -}}
# retry <max_attempts> <sleep_seconds> <description> -- <command...>
# Wrap pipes/redirects in a shell function and pass that function name.
retry() {
  local max=$1
  local sleep_s=$2
  local desc=$3
  shift 3
  if [ "$1" = "--" ]; then shift; fi
  local i=1
  while true; do
    if "$@"; then
      return 0
    fi
    if [ "$i" -ge "$max" ]; then
      echo "FAIL: $desc — exhausted $max attempts" >&2
      return 1
    fi
    echo "Transient: $desc failed (attempt $i/$max), sleeping ${sleep_s}s..." >&2
    sleep "$sleep_s"
    i=$((i + 1))
  done
}
{{- end -}}
