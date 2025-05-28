package mymodule

apiVersion: "argoproj.io/v1alpha1"
kind:       "WorkflowTemplate"
metadata: name: "capture-replay"
spec: {
  entrypoint:         "run-all"
  serviceAccountName: "argo-workflow-executor"

	let RUN_ALL = (#WFTemplate.Dag & {
		#parameters: {
			sessionName: { type: string, requiredArg: true }
			proxyDestination: { type: string, requiredArg: true }
			proxyListenPort: { type: int, requiredArg: true }
			replayerTargetConfig: { type: string, requiredArg: true }

			sourceConfig: type: string
			providedKafkaBootstrapServers: type: string
			providedKafkaK8sName: type: string
			kafkaPrefix: type: string
			topicName: type: string
			topicPartition: type: string
		}
		dag: [
			{
				name: ID_GENERATOR.name
				template: name
				arguments: parameters: {
					(#ProxyInputsIntoArguments & {#in: _paramsWithTemplatePathsMap}).out
				}
			}
		]
	})

	let GET_BROKERS = (#WFTemplate.Steps & {

	})

	let GET_USER_CONFIRMATION = (#WFTemplate.Suspend & {

	})

	let ID_GENERATOR = (#WFTemplate.DoNothing & {
		#parameters: {
			proxyEndpoint: type: string
			serviceName: type: string
		}

	})



  templates: [
    RUN_ALL, GET_BROKERS, GET_USER_CONFIRMATION, ID_GENERATOR
  ]
}
