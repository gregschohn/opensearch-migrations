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

		  PARAMS=#inputParams: {
				image:    { passToContainer: false, parameterValue: "migrations/traffic_replayer:latest" }
				replicas: { passToContainer: false, type: int }

				targetUrl: { requiredArg: true, type: string }

				packetTimeoutSeconds:      parameterValue: 70
				maxConcurrentRequests:     parameterValue: 1024
				numClientThreads:          parameterValue: 0
				lookaheadTimeWindow:       parameterValue: 300
				speedupFactor:             parameterValue: 1.1
				targetResponseTimeout:     parameterValue: 30
				insecure:                  parameterValue: false
				kafkaTrafficEnableMskAuth: parameterValue: false

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
			#manifest: spec: replicas: (#InlineInputParameter & {name: "replicas", params: #inputParams}).out
  		#containers: [
	  		#Container.#Jib & {
			  	name:        "replayer"
		  		#inputParams: PARAMS
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