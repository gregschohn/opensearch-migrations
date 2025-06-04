package mymodule

#MIGRATION_TEMPLATES: CAPTURE_REPLAY: {

apiVersion: "argoproj.io/v1alpha1"
kind:       "WorkflowTemplate"
metadata: name: "capture-replay"
spec: {
  entrypoint:         "run-all"
  serviceAccountName: "argo-workflow-executor"

	let RUN_ALL = (#WFTemplate.#Dag & {
		name: entrypoint
		#parameters: {
			sessionName:          { requiredArg: true, type: string }
			proxyDestination:     { requiredArg: true, type: string }
			proxyListenPort:      { requiredArg: true, type: int }
			replayerTargetConfig: { requiredArg: true, type: string }

			sourceConfig:                  type: string
			providedKafkaBootstrapServers: type: string
			providedKafkaK8sName:          type: string
			kafkaPrefix:                   type: string
			topicName:                     type: string
			topicPartition:                type: string
		}
		dag:
		 tasks: [
			{
				name: ID_GENERATOR.name
				template: name
				_paramsWithTemplatePathsMap: _
				arguments: parameters: {
					(#ProxyInputsIntoArguments & {#in: _paramsWithTemplatePathsMap}).out
				}
			}
		]
	})

	let GET_BROKERS = (#WFTemplate.#Steps & {
		name: "get-brokers"
	})

	let GET_USER_CONFIRMATION = (#WFTemplate.#Suspend & {
		name: "get-user-confirmation"
	})

	let ID_GENERATOR = (#WFTemplate.#DoNothing & {
		name: "id-generator"
		#parameters: {
			proxyEndpoint: type: string
			serviceName:   type: string
		}

	})



  templates: [
    RUN_ALL, GET_BROKERS, GET_USER_CONFIRMATION, ID_GENERATOR
  ]
}
}