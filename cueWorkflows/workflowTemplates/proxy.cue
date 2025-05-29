package mymodule

apiVersion: "argoproj.io/v1alpha1"
kind:       "WorkflowTemplate"
metadata: name: "capture-proxy"
spec: {
  entrypoint:         "deploy-capture-proxy"
  serviceAccountName: "argo-workflow-executor"

  let DS = (#WFTemplate.Steps & {
    name:                        "deploy-service"
    #parameters: CS.#parameters
    _paramsWithTemplatePathsMap: _

    outputs: parameters: [{
      name: "endpoint"
      valueFrom: expression: "steps['\(CS.name)'].outputs.parameters['endpoint'] + ':' + \(_paramsWithTemplatePathsMap.frontsidePort.exprInputPath)"
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
    #parameters:                 {
    	frontsidePort: { type: int }
    	serviceName: { defaultValue: "capture-proxy"}
    }
    _paramsWithTemplatePathsMap: _

    outputs: parameters: [{
      name: "endpoint"
      valueFrom: jsonPath: "{.metadata.name}"
    }]

    #manifest: {
      apiVersion: "v1"
      kind:       "Service"
      metadata: name: (#ForInputParameter & {name: "serviceName", params: #parameters}).out
      labels: app:    "proxy"
      spec: selector: app: "proxy" // Selector should match pod labels directly, not use matchLabels
      ports: [{
        port:       (#ForInputParameter & {name: "frontsidePort", params: #parameters}).out
        targetPort: (#ForInputParameter & {name: "frontsidePort", params: #parameters}).out
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
      #Container.Jib & {
        name:              "proxy"
        #parameters: PARAMS
        #ports: [{
          containerPort: (#ForInputParameter & {name: "frontsidePort", params: PARAMS}).out
        }]
      },
    ]
    let PARAMS=#parameters
    #parameters: {
      backsideUriString: type: string
      frontsidePort: type:     int

      image: { defaultValue: "migrations/migration-console:latest", passToContainer: false}
      initImage: { defaultValue: "migrations/migration-console:latest", passToContainer: false}
      replicas: { type: int, defaultValue: 1, passToContainer: false}

      traceDirectory: type:                     string
      noCapture: type:                          bool
      kafkaPropertiesFile: type:                string
      kafkaClientId: type:                      string
      kafkaConnection: type:                    string
      kafkaTopic: type:                         string
      mskAuthEnabled: type:                     bool
      sslConfigFilePath: type:                  string
      maximumTrafficStreamSize: type:           string
      allowInsecureConnectionsToBackside: type: bool
      numThreads: type:                         string
      destinationConnectionPoolSize: type:      string
      destinationConnectionPoolTimeout: type:   string
      otelCollectorEndpoint: { defaultValue: "http://otel-collector:4317"}
      headerOverrides: type:            string
      suppressCaptureHeaderPairs: type: string
    }
    _paramsWithTemplatePathsMap: _

    #manifest: spec: replicas: (#ForInputParameter & {name: "replicas", params: #parameters}).out
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
