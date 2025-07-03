package mymodule

#MIGRATION_TEMPLATES: PROXY: #K8sWorkflowTemplate & {
  entrypoint:         "deploy-capture-proxy"
  serviceAccountName: "argo-workflow-executor"

  #name: "capture-proxy"
 	_templateSignaturesMap: [string]: #TemplateSignature
 	_sharedServiceParameters: {
		frontsidePort:  { requiredArg: true, type: int }
		serviceName:    defaultValue: "capture-proxy"
	}
	_sharedServiceParameters: [string]: #TemplateParameterDefinition
	#templates: {
    deployService: (#WFSteps & {
      #parameters: _sharedServiceParameters
      _parsedParams: { ... }

      outputs: parameters: [{
        name: "endpoint"
//          valueFrom: expression: "steps['\(CS.name)'].outputs.parameters['endpoint'] + ':' + \(_parsedParams._parameterMap.frontsidePort.parameterPath)"
      }]

      steps: [[{
        #templateSignature: _templateSignaturesMap.createService
				#argumentMappings: { for k, v in _parsedParams.parameterMap { "\(k)": paramWithName: v } }
      }]]
    }),

	  createService: (#WFResource & {
      #manifestSchema: {} // TODO - Find the right schema for a service and use that instead!
      #parameters: _sharedServiceParameters

      outputs: parameters: [
      	{ name: "endpoint",    valueFrom: jsonPath:   "{.metadata.name}" },
//      	{ name: "endpointUrl", valueFrom: expression: "inputs.parameters['frontside-port']" }
      ]

      #manifest: {
        apiVersion: "v1"
        kind:       "Service"
        metadata: name: (#InlineInputParameter & {name: "serviceName", params: #parameters}).out
        labels: app:    "proxy"
        spec: selector: app: "proxy" // Selector should match pod labels directly, not use matchLabels
        ports: [{
          port:       (#InlineInputParameter & {name: "frontsidePort", params: #parameters}).out
          targetPort: (#InlineInputParameter & {name: "frontsidePort", params: #parameters}).out
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
          #parameters: PARAMS
          #ports: [{
            containerPort: (#InlineInputParameter & {name: "frontsidePort", params: PARAMS}).out
          }]
        },
      ]
      let PARAMS=#parameters
      #parameters: {
        backsideUriString: { requiredArg: true, type: string }
        frontsidePort:     { requiredArg: true, type: int }

        image:     { passToContainer: false, defaultValue: "migrations/migration-console:latest" }
        replicas:  { passToContainer: false, defaultValue: 1 }

  		  otelCollectorEndpoint: defaultValue: "http://otel-collector:4317"

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

      #manifest: spec: replicas: (#InlineInputParameter & {name: "replicas", params: #parameters}).out
      resource: {
        setOwnerReference: true
        action:            "create"
      }
    })
  }
}
