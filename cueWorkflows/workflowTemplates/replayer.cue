package mymodule

#MIGRATION_TEMPLATES: REPLAYER: {
apiVersion: "argoproj.io/v1alpha1"
kind:       "WorkflowTemplate"
metadata: name: "replayer"
spec: {
  entrypoint:         "deploy-replayer"
  serviceAccountName: "argo-workflow-executor"

	templates: [
		(#WFDeployment & {
	    name:         entrypoint
	    #resourceName: name

		  PARAMS=#parameters: {
				image:    { passToContainer: false, defaultValue: "migrations/traffic_replayer:latest" }
				replicas: { passToContainer: false, type: int }

				targetUrl: { requiredArg: true, type: string }

				packetTimeoutSeconds:      defaultValue: 70
				maxConcurrentRequests:     defaultValue: 1024
				numClientThreads:          defaultValue: 0
				lookaheadTimeWindow:       defaultValue: 300
				speedupFactor:             defaultValue: 1.1
				targetResponseTimeout:     defaultValue: 30
				insecure:                  defaultValue: false
				kafkaTrafficEnableMskAuth: defaultValue: false

				removeAuthHeader:              type: bool
				sigv4AuthHeaderServiceRegion:  type: string
				userAgent:                     type: string
				transformerConfig:             type: string
				tupleTransformerConfig:        type: string
				kafkaTrafficBrokers:           type: string
				kafkaTrafficGroupId:           type: string
				kafkaTrafficTopic:             type: string
				kafkaTrafficProperties:        type: string
			}
			_paramsWithTemplatePathsMap: _
			#manifest: spec: replicas: (#InlineInputParameter & {name: "replicas", params: #parameters}).out
  		#containers: [
	  		#Container.#Jib & {
			  	name:        "replayer"
		  		#parameters: PARAMS
	  		}
  		]
  		resource: {
      	setOwnerReference: true
      	action:            "create"
  		}
		})
	]
}
}