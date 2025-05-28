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
				targetUrl: { type: "string", requiredArg: true }

				image: { type: "string", defaultValue: "migrations/traffic_replayer:latest", passToContainer: false }
				replicas: { type: "int", passToContainer: false }

				packetTimeoutSeconds: { type: "int", defaultValue: 70 }
				maxConcurrentRequests: { type: "int", defaultValue: 1024 }
				numClientThreads: { type: "int", defaultValue: 0 }
				lookaheadTimeWindow: { type: "int", defaultValue: 300 }
				speedupFactor: { type: "float", defaultValue: 1.1 }
				targetResponseTimeout: { type: "int", defaultValue: 30 }
				insecure: { type: "bool", defaultValue: false }
				kafkaTraficEnableMskAuth: { type: "bool", defaultValue: false }

				removeAuthHeader: type: "bool"
				sigv4AuthHeaderServiceRegion: type: "string"
				userAgent: type: "string"
				transformerConfig: type: "string"
				tupleTransformerConfig: type: "string"
				kafkaTraficBrokers: type: "string"
				kafkaTraficGroupId: type: "string"
				kafkaTraficTopic: type: "string"
				kafkaTraficProperties: type: "string"
			}
			_paramsWithTemplatePathsMap: _
			#manifest: spec: replicas: (#ForInputParameter & {name: "replicas", params: #parameters}).out
  		#containers: [
	  		#Container.Jib & {
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