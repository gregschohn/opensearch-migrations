/**
 * CDC-specific Kubernetes cleanup before eksCleanupStep.
 *
 * Runs 'workflow reset --all --include-proxies --delete-storage' via the migration
 * console CLI to cleanly delete all CDC CRDs (captureproxies, trafficreplays,
 * snapshotmigrations, kafkaclusters) along with their owned child resources
 * (services, deployments, certificates, Kafka topics/users/nodepools) and Kafka
 * PVCs. Then invokes 'pipenv run app --delete-only' which handles the full MA
 * helm teardown (including a belt-and-suspenders storage cleanup).
 *
 * Call this BEFORE eksCleanupStep, which handles namespace deletion, instance
 * profiles, and orphaned security groups.
 *
 * Usage:
 *   cdcCleanupStep(kubeContext: env.eksKubeContext)
 */
def call(Map config = [:]) {
    def kubeContext = config.kubeContext
    if (!kubeContext) { error("cdcCleanupStep: 'kubeContext' is required") }

    dir('libraries/testAutomation') {
        sh "pipenv install --deploy"
        sh "kubectl --context=${kubeContext} -n ma get pods || true"
        // Run 'workflow reset' with --delete-storage while the migration-console pod
        // is still alive so the CLI can find and delete Kafka PVCs. This prevents
        // cluster-ID conflicts on the next pipeline run that reuses the namespace.
        // The reset CLI also removes captureproxy services/deployments/certificates
        // and strips stuck finalizers on Kafka/KafkaNodePool CRs.
        sh """
          kubectl --context=${kubeContext} -n ma exec migration-console-0 -- \\
            /bin/bash -lc 'workflow reset --all --include-proxies --delete-storage' || true
        """
        sh "kubectl --context=${kubeContext} -n ma delete workflows --all --timeout=60s || true"
        sh "pipenv run app --delete-only --kube-context=${kubeContext}"
        echo "Resources remaining in 'ma' namespace after CDC cleanup (expected to be empty):"
        sh """
          kubectl --context=${kubeContext} api-resources --namespaced=true -o name --verbs=list 2>/dev/null | \\
            grep -v '^events' | sort -u | \\
            xargs -n1 -I{} sh -c 'echo "=== {} ==="; kubectl --context='"${kubeContext}"' get {} -n ma --ignore-not-found 2>/dev/null' | \\
            awk '/^=== / {hdr=\$0; buf=""; next} {buf = buf ? buf ORS \$0 : \$0} END{if(buf) print hdr ORS buf}' || true
        """
        echo "Orphaned PVs still bound to 'ma' namespace (expected to be empty):"
        sh """
          kubectl --context=${kubeContext} get pv -o json 2>/dev/null | \\
            jq -r '.items[]? | select(.spec.claimRef.namespace == "ma") | .metadata.name' || true
        """
    }
}
