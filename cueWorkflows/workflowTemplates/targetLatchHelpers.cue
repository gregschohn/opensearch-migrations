package mymodule

_resource_targetLatchHelpers_init_sh: string @tag(resource_targetLatchHelpers_init_sh)
_resource_targetLatchHelpers_decrement_sh: string @tag(resource_targetLatchHelpers_decrement_sh)
_resource_targetLatchHelpers_cleanup_sh: string @tag(resource_targetLatchHelpers_cleanup_sh)

#MIGRATION_TEMPLATES: TARGET_LATCH_HELPERS: #K8sWorkflowTemplate & {
	#name: "target-latch-helpers"
	#templates: {
		init: (#WFContainer & {
	  	#parameters: {
	  		configurations: { requiredArg: true, type: [...#SOURCE_MIGRATION_CONFIG] },
			 	targets:        { requiredArg: true, type: [ ...#CLUSTER_CONFIG ] },
			 	prefix:         { requiredArg: true, type: string }
 			}

  		container: command: [
  			"/bin/sh",
			 	"-c",
				(#DecodeBase64 & {in: _resource_targetLatchHelpers_init_sh}).out
		  ]
		 })

  	decrement: (#WFScript & {
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

  	cleanup: (#WFContainer & {
		 	#parameters: {
		 		prefix: type: string
			}
			container: command: [
			 	"/bin/sh",
  			"-c",
	  		(#DecodeBase64 & {in: _resource_targetLatchHelpers_cleanup_sh}).out
	 		]
	 	})
	}
}
