package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"

#MIGRATION_TEMPLATES: FULL: {

argo.#."io.argoproj.workflow.v1alpha1.WorkflowTemplate" & {

apiVersion: "argoproj.io/v1alpha1"
kind:       "WorkflowTemplate"
metadata: name: "full-migration"
spec: #Spec & {
  entrypoint:         "main"
  serviceAccountName: "argo-workflow-executor"
  parallelism: 100
  _workflowParameters: _ // pull this processed value into the scope for other templates to pull from

  let MAIN = (#WFTemplate.#Steps & {
    name:                        "main"
		let s3ParamMap = _workflowParameters.s3SnapshotConfigMap.templateInputPath
		let s3ParamToConfigMapKeyMapping = {
			s3AwsRegion: "AWS_REGION",
			s3Endpoint:  "ENDPOINT",
			s3RepoUri:   "repo_uri"
		}
    #parameters: {
    	sourceMigrationConfigs: type: [ ...#SOURCE_MIGRATION_CONFIG ],
    	targets:                type: [ ...#CLUSTER_CONFIG ],
    	{
    		for k,v in #S3_PARAMS {
	  			"\(k)": defaultValue: #Parameters.#FromConfigMap & { #type: string, map: s3ParamMap, key: s3ParamToConfigMapKeyMapping[k] }
			  }
			}
    }
    _paramsWithTemplatePathsMap: _

    steps: [[
      {
      	name: "foo"
      	template: "foo"
//        #args: [string]: #Parameters.#ValueFiller
//		  	let s3ParamMap = _workflowParameters.s3SnapshotConfigMap.templateInputPath
        #args: {
//					{ to: "#S3_PARAMS.s3AwsRegion",  map: s3ParamMap, key: "AWS_REGION" },
//					{ to: "#S3_PARAMS.s3Endpoint", map: s3ParamMap, key: "ENDPOINT" },
//					{ to: "#S3_PARAMS.s3RepoUri",    map: s3ParamMap, key: "repo_uri" }
				}
			}
    ]]
  })

  templates: [
    MAIN,
  ]
}
}
}
