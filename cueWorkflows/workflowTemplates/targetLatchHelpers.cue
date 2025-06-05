package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"
import base64 "encoding/base64"

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

    #parameters: {
    	configurations: type: [...#SOURCE_MIGRATION_CONFIG]
    	targets:        type: [ ...#CLUSTER_CONFIG ],
    	prefix:         type: string
    }

    container: command: [
    	"/bin/sh",
    	"-c",
      "\(base64.Decode(null, _resource_targetLatchHelpers_init_sh))"
    ]
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
    	source: "\(base64.Decode(null, _resource_targetLatchHelpers_decrement_sh))"
    }
  })

  let CLEANUP = (#WFTemplate.#Container & {
    name: "init"
    #parameters: {
    	prefix: type: string
    }
    _paramsWithTemplatePathsMap: _
		container: command: [
    	"/bin/sh",
    	"-c",
    	"\(base64.Decode(null, _resource_targetLatchHelpers_cleanup_sh))"
    ]

  })

  templates: [
    INIT, DECREMENT, CLEANUP
  ]
}
}
}
