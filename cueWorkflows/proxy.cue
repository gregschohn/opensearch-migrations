package mymodule

apiVersion: "argoproj.io/v1alpha1"
kind: "WorkflowTemplate"
metadata:
  name: "capture-proxy"
spec:
  entrypoint: "deploy-capture-proxy"
  serviceAccountName: "argo-workflow-executor"

	#serviceParameters: {
			frontsidePort: { type: "int" },
			serviceName: { type: "string", defaultValue: "capture-proxy" }
	}

  templates: [
		(#DeploymentTemplate & {
				#containerCommand: "/runJavaWithClasspath.sh org.opensearch.CaptureProxy"
				#ports: containerPort: "\(_paramsWithTemplatePathsMap.frontsidePort.templateInputPath)"
				#parameters: {
						backsideUriString: {type: "string"}
						frontsidePort: {type: "int"}

						image: { type: "string", defaultValue: "migrations/migration-console:latest" }
						initImage: {type: "string", defaultValue: "migrations/migration-console:latest" }
						replicas: {type: "int", defaultValue: 1 }

						traceDirectory: {type: "string" }
						noCapture: {type: "bool" }
						kafkaPropertiesFile: {type: "string" }
						kafkaClientId: {type: "string" }
						kafkaConnection: {type: "string" }
						kafkaTopic: {type: "string" }
						mskAuthEnabled: {type: "bool" }
						sslConfigFilePath: {type: "string" }
						maximumTrafficStreamSize: {type: "string" }
						allowInsecureConnectionsToBackside: {type: "bool" }
						numThreads: {type: "string" }
						destinationConnectionPoolSize: {type: "string" }
						destinationConnectionPoolTimeout: {type: "string" }
						otelCollectorEndpoint: {type: "string", defaultValue: "http://otel-collector:4317" }
						headerOverrides: {type: "string" }
						suppressCaptureHeaderPairs: {type: "string" }
				}
				_paramsWithTemplatePathsMap: _

				name: "deploy-capture-proxy"
				resource: setOwnerReference: true
		}),

		(#ResourceTemplate & {
				name: "create-service",
				#parameters: #serviceParameters
				_paramsWithTemplatePathsMap: _

				successCondition: "status.loadBalancer.ingress",
				resource: {
						flags: [ "--validate=false" ]
						setOwnerReference: true
						manifest:
							"""
							apiVersion: v1
							kind: Service
							metadata:
								name: \(_paramsWithTemplatePathsMap.serviceName.templateInputPath)
								labels:
									app: proxy
							spec:
								selector:
									app: proxy  # Selector should match pod labels directly, not use matchLabels
								ports:
								- port: \(_paramsWithTemplatePathsMap.frontsidePort.templateInputPath)
								  targetPort: \(_paramsWithTemplatePathsMap.frontsidePort.templateInputPath)
								type: LoadBalancer
							"""
						}
				}),

				(#StepTemplate & {
						name: "deploy-service"
						#parameters: #serviceParameters
						_paramsWithTemplatePathsMap: _
						steps: [
							{
								N=name: "create-service"
								template: N
								arguments: {
									  parameters: #ProxyInputsIntoArguments & {#in: _paramsWithTemplatePathsMap }
									}
							}
						]
				})
	]
