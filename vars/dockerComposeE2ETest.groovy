def call(Map config = [:]) {
    def jobName = config.jobName ?: "docker-compose-e2e-test"

    pipeline {
        agent { label config.workerAgent ?: 'Jenkins-Default-Agent-X64-C5xlarge-Single-Host' }

        parameters {
            string(name: 'GIT_REPO_URL', defaultValue: 'https://github.com/opensearch-project/opensearch-migrations.git', description: 'Git repository url')
            string(name: 'GIT_BRANCH', defaultValue: 'main', description: 'Git branch to use for repository')
            string(name: 'GIT_COMMIT', defaultValue: '', description: '(Optional) Specific commit to checkout after cloning branch')
        }

        options {
            timeout(time: 30, unit: 'MINUTES')
            buildDiscarder(logRotator(daysToKeepStr: '30'))
            skipDefaultCheckout(true)
        }

        triggers {
            GenericTrigger(
                    genericVariables: [
                            [key: 'GIT_REPO_URL', value: '$.GIT_REPO_URL'],
                            [key: 'GIT_BRANCH', value: '$.GIT_BRANCH'],
                            [key: 'GIT_COMMIT', value: '$.GIT_COMMIT'],
                            [key: 'job_name', value: '$.job_name']
                    ],
                    tokenCredentialId: 'jenkins-migrations-generic-webhook-token',
                    causeString: 'Triggered by PR on opensearch-migrations repository',
                    regexpFilterExpression: "^${jobName}\$",
                    regexpFilterText: '$job_name'
            )
        }

        stages {
            stage('Checkout') {
                steps {
                    checkoutStep(branch: params.GIT_BRANCH, repo: params.GIT_REPO_URL, commit: params.GIT_COMMIT)
                }
            }

            // No-op placeholder. The real docker-compose E2E stages (build images,
            // compose up, run replayer_tests.py, archive logs, compose down) are
            // intentionally omitted here so the Jenkins job exists, the GHA
            // jenkins-trigger action can reach it, and the webhook/trigger wiring
            // is exercised end-to-end. A follow-up PR will swap in the real stages
            // once this scaffolding has landed on main and the Jenkins-side jobs
            // (pr-docker-compose-e2e-test / main-docker-compose-e2e-test) are
            // registered on migrations.ci.opensearch.org.
            stage('No-op placeholder') {
                steps {
                    echo 'docker-compose-e2e-test pipeline is a scaffolding no-op; real stages will be added in a follow-up PR.'
                }
            }
        }
    }
}
