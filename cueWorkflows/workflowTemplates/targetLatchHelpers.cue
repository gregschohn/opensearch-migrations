package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"

_resource_targetLatchHelpers_init_sh: string @tag(resource_targetLatchHelpers_init_sh)
_resource_targetLatchHelpers_decrement_sh: string @tag(resource_targetLatchHelpers_decrement_sh)
_resource_targetLatchHelpers_cleanup_sh: string @tag(resource_targetLatchHelpers_cleanup_sh)

#MIGRATION_TEMPLATES: TARGET_LATCH_HELPERS: {

argo.#."io.argoproj.workflow.v1alpha1.WorkflowTemplate" & {

apiVersion: "argoproj.io/v1alpha1"
kind:       "WorkflowTemplate"
metadata: name: "target-latch-helpers"
spec: #Spec & {
  let INIT = (#WFTemplate.#Container & {
    name: "init"

    let PARAMS = {
    	configurations: type: [...#SOURCE_MIGRATION_CONFIG]
    	targets:        type: [ ...#CLUSTER_CONFIG ],
    	prefix:         type: string
    	//etcdEndpoint:   default
    }
    #parameters: PARAMS

    container: {
    	command: [
					"/bin/sh",
					"-c",
					(#DecodeBase64 & {in: _resource_targetLatchHelpers_init_sh}).out
			],
			#parameters: PARAMS
    }
    _paramsWithTemplatePathsMap: _
  })

  let DECREMENT = (#WFTemplate.#Script & {
  	name: "decrement"
    #parameters: {
    	processor: type: string
    	target:    type: string
    	prefix:    type: string
    }
    _paramsWithTemplatePathsMap: _
    script: {
    	image: (#InlineInputParameter & {name: "etcdImage", params: #WORKFLOW_PARAMS}).out,
    	source: (#DecodeBase64 & {in: _resource_targetLatchHelpers_decrement_sh}).out
    }
  })

  let CLEANUP = (#WFTemplate.#Container & {
    name: "cleanup"
    #parameters: {
    	prefix: type: string
    }
    _paramsWithTemplatePathsMap: _
		container: command: [
    	"/bin/sh",
    	"-c",
    	(#DecodeBase64 & {in: _resource_targetLatchHelpers_cleanup_sh}).out
    ]

  })

  templates: [
    INIT, DECREMENT, CLEANUP
  ]
}
}
}
