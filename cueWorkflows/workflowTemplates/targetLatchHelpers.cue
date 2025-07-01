package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"

_resource_targetLatchHelpers_init_sh: string @tag(resource_targetLatchHelpers_init_sh)
_resource_targetLatchHelpers_decrement_sh: string @tag(resource_targetLatchHelpers_decrement_sh)
_resource_targetLatchHelpers_cleanup_sh: string @tag(resource_targetLatchHelpers_cleanup_sh)

#MIGRATION_TEMPLATES: TARGET_LATCH_HELPERS: {

  #INIT: (#WFContainer & {
    name: "init"

    #parameters: {
    	configurations: { requiredArg: true, type: [...#SOURCE_MIGRATION_CONFIG] },
    	targets:        { requiredArg: true, type: [ ...#CLUSTER_CONFIG ] },
    	prefix:         { requiredArg: true, type: string }
    	//etcdEndpoint:   default
    }

    container: command: [
    	"/bin/sh",
    	"-c",
      (#DecodeBase64 & {in: _resource_targetLatchHelpers_init_sh}).out
    ]
  })

  let DECREMENT = (#WFScript & {
  	name: "decrement"
    #parameters: {
    	processor: type: string
    	target:    type: string
    	prefix:    type: string
    }
    script: {
    	image: (#InlineInputParameter & {name: "etcdImage", params: #WORKFLOW_PARAMS}).out,
    	source: (#DecodeBase64 & {in: _resource_targetLatchHelpers_decrement_sh}).out
    }
  })

  let CLEANUP = (#WFContainer & {
    name: "cleanup"
    #parameters: {
    	prefix: type: string
    }
		container: command: [
    	"/bin/sh",
    	"-c",
    	(#DecodeBase64 & {in: _resource_targetLatchHelpers_cleanup_sh}).out
    ]
  })


  argo.#."io.argoproj.workflow.v1alpha1.WorkflowTemplate" & {
		apiVersion: "argoproj.io/v1alpha1"
		kind:       "WorkflowTemplate"
		metadata: name: "target-latch-helpers"

		spec: #Spec & {
		templates: [ #INIT, DECREMENT, CLEANUP ]
		}
	}
}
