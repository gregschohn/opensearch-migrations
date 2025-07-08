package mymodule

#MIGRATION_TEMPLATES: PROXY: #K8sWorkflowTemplate & {
  entrypoint:         "deploy-capture-proxy"
  serviceAccountName: "argo-workflow-executor"

  #name: "capture-proxy"
 	_templateSignaturesMap: [string]: #TemplateSignature
 	_sharedServiceParameters: {
		frontsidePort:  { requiredArg: true, type: int }
		serviceName:    parameterValue: "capture-proxy"
	}
	_sharedServiceParameters: [string]: #TemplateParameterDefinition
	#templates: {
    deployService: (#WFSteps & {
      #inputParams: _sharedServiceParameters
      _parsedInputParams: { ... }

      #outputParams: {
        endpoint: { expression: #Expr.Concat & {list: [
//        	  { argoReadyString: "steps['createService'].outputs.parameters['endpoint']" },
//          	":",
//         		{ paramWithName: _parsedInputParams._parameterMap.frontsidePort }
       	  ]}
       	}
      }

      steps: [[{
        #templateSignature: _templateSignaturesMap.createService
				#argumentMappings: { for k, v in _parsedInputParams.parameterMap { "\(k)": paramWithName: v } }
      }]]
    }),

	  createService: (#WFResource & {
      #manifestSchema: {} // TODO - Find the right schema for a service and use that instead!
      #inputParams: _sharedServiceParameters

      outputs: parameters: [
      	{ name: "endpoint",    valueFrom: jsonPath:   "{.metadata.name}" },
      ]

      #manifest: {
        apiVersion: "v1"
        kind:       "Service"
        metadata: name: (#InlineInputParameter & {name: "serviceName", params: #inputParams}).out
        labels: app:    "proxy"
        spec: selector: app: "proxy" // Selector should match pod labels directly, not use matchLabels
        ports: [{
          port:       (#InlineInputParameter & {name: "frontsidePort", params: #inputParams}).out
          targetPort: (#InlineInputParameter & {name: "frontsidePort", params: #inputParams}).out
        }]
        type: "LoadBalancer"
      }
      resource: {
        successCondition: "status.loadBalancer.ingress"
        flags: ["--validate=false"]
        setOwnerReference: true
        action:            "create"
      }
    }),

    proxy: (#WFDeployment & {
      #resourceName: "proxy"
      #containers: [
        #Container.#Jib & {
          name:              "proxy"
          #inputParams: PARAMS
          #ports: [{
            containerPort: (#InlineInputParameter & {name: "frontsidePort", params: PARAMS}).out
          }]
        },
      ]
      let PARAMS=#inputParams
      #inputParams: {
        backsideUriString: { requiredArg: true, type: string }
        frontsidePort:     { requiredArg: true, type: int }

        image:     { passToContainer: false, parameterValue: "migrations/migration-console:latest" }
        replicas:  { passToContainer: false, parameterValue: 1 }

  		  otelCollectorEndpoint: parameterValue: "http://otel-collector:4317"

        traceDirectory:                     type: string
        noCapture:                          type: bool
        kafkaPropertiesFile:                type: string
        kafkaClientId:                      type: string
        kafkaConnection:                    type: string
        kafkaTopic:                         type: string
        mskAuthEnabled:                     type: bool
        sslConfigFilePath:                  type: string
        maximumTrafficStreamSize:           type: string
        allowInsecureConnectionsToBackside: type: bool
        numThreads:                         type: string
        destinationConnectionPoolSize:      type: string
        destinationConnectionPoolTimeout:   type: string
        headerOverrides:                    type: string
        suppressCaptureHeaderPairs:         type: string
      }

      #manifest: spec: replicas: (#InlineInputParameter & {name: "replicas", params: #inputParams}).out
      resource: {
        setOwnerReference: true
        action:            "create"
      }
    })
  }
}
