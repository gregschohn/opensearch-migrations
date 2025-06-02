package mymodule

apiVersion: "argoproj.io/v1alpha1"
kind:       "WorkflowTemplate"
metadata: name: "replayer"
spec: {
  entrypoint:         "deploy-replayer"
  serviceAccountName: "argo-workflow-executor"

	templates: [
		(#WFTemplate.#Deployment & {
	    name:         entrypoint
	    #resourceName: name

		  PARAMS=#parameters:          {
				image:    { passToContainer: false, defaultValue: "migrations/traffic_replayer:latest" }
				replicas: { passToContainer: false, type: int }

				targetUrl: { requiredArg: true, type: string }

				packetTimeoutSeconds:     defaultValue: 70
				maxConcurrentRequests:    defaultValue: 1024
				numClientThreads:         defaultValue: 0
				lookaheadTimeWindow:      defaultValue: 300
				speedupFactor:            defaultValue: 1.1
				targetResponseTimeout:    defaultValue: 30
				insecure:                 defaultValue: false
				kafkaTraficEnableMskAuth: defaultValue: false

				removeAuthHeader:             type: bool
				sigv4AuthHeaderServiceRegion: type: string
				userAgent:                    type: string
				transformerConfig:            type: string
				tupleTransformerConfig:       type: string
				kafkaTraficBrokers:           type: string
				kafkaTraficGroupId:           type: string
				kafkaTraficTopic:             type: string
				kafkaTraficProperties:        type: string
			}
			_paramsWithTemplatePathsMap: _
			#manifest: spec: replicas: (#ForInputParameter & {name: "replicas", params: #parameters}).out
  		#containers: [
	  		#Container.#Jib & {
			  	name:              "replayer"
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