package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"

argo.#."io.argoproj.workflow.v1alpha1.WorkflowTemplate" & {

	apiVersion: "argoproj.io/v1alpha1"
kind:       "WorkflowTemplate"
metadata: name: "full-migration"
spec: #Spec & {
  entrypoint:         "main"
  serviceAccountName: "argo-workflow-executor"
  parallelism: 100

  #workflowParameters: {
  		etcdEndpoints:       defaultValue: "http://etcd.ma.svc.cluster.local:2379"
  		etcdUser:            defaultValue: "root"
  		etcdPassword:        defaultValue: "password"
  		etcdImage:           defaultValue: "migrations/migration_console:latest"
  		demoMaxSleepTime:    defaultValue: 60
  		s3SnapshotConfigMap: defaultValue: "s3-snapshot-config"
  }

  let MAIN = (#WFTemplate.#Steps & {
    name:                        "main"
    #parameters: {
    	sourceMigrationConfigs: type: [...#SOURCE_MIGRATION_CONFIG]
    	targets:                type: [...#CLUSTER_CONFIG]
    }
    _paramsWithTemplatePathsMap: _

    steps: [[
      {
        name:      "get-configs-from-maps"
        template:  name
        #args: {
					awsRegion:  { fromWorkflowConfig:"" }
					s3Endpoint: { fromWorkflowConfig:"" }
					repoUri:    { fromWorkflowConfig:"" }
					}
			}
    ]]
  })

  templates: [
    MAIN
  ]
}
}