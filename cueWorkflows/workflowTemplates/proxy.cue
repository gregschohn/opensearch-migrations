package mymodule

#MIGRATION_TEMPLATES: PROXY: {
apiVersion: "argoproj.io/v1alpha1"
kind:       "WorkflowTemplate"
metadata: name: "capture-proxy"
spec: {
  entrypoint:         "deploy-capture-proxy"
  serviceAccountName: "argo-workflow-executor"

  let DS = (#WFTemplate.#Steps & {
    name:                        "deploy-service"
    #parameters: CS.#parameters
    _paramsWithTemplatePathsMap: _

    outputs: parameters: [{
      name: "endpoint"
      valueFrom: expression: "steps['\(CS.name)'].outputs.parameters['endpoint'] + ':' + \(_paramsWithTemplatePathsMap.frontsidePort.parameterPath)"
    }]

    steps: [[
      {
        name:      "create-service"
        template:  name
				arguments: parameters: (#ProxyInputsIntoArguments & {#in: _paramsWithTemplatePathsMap}).out
      },
    ]]
  })

  let CS = (#WFTemplate.#Resource & {
    name:                        "create-service"
    #manifestSchema: {} // TODO - Find the right schema for a service and use that instead!
    #parameters:                 {
    	frontsidePort:  { requiredArg: true, type: int }
    	serviceName:    defaultValue: "capture-proxy"
    }
    _paramsWithTemplatePathsMap: _

    outputs: parameters: [{
      name: "endpoint"
      valueFrom: jsonPath: "{.metadata.name}"
    }]

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
  })

  let P = (#WFTemplate.#Deployment & {
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
    _paramsWithTemplatePathsMap: _

    #manifest: spec: replicas: (#InlineInputParameter & {name: "replicas", params: #parameters}).out
    name: "deploy-capture-proxy"
    resource: {
      setOwnerReference: true
      action:            "create"
    }
  })

  templates: [
    DS, CS, P,
  ]
}
}