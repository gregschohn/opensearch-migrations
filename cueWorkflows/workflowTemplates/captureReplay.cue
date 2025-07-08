package mymodule

#MIGRATION_TEMPLATES: CAPTURE_REPLAY: #K8sWorkflowTemplate & {
  entrypoint:         "runAll"
  serviceAccountName: "argo-workflow-executor"

	#name: "captureReplay"
	_templateSignaturesMap: [string]: #TemplateSignature
	#templates: {
  	runAll: (#WFDag & {
			#inputParams: {
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
  		_parsedInputParams: {...}

			dag: {
			  tasks: [{
						#templateSignature: _templateSignaturesMap.idGenerator
					  #argumentMappings: {
							serviceName: paramWithName: #ParameterWithName & _parsedInputParams.parameterMap.sessionName
							//proxyEndpoint: expression:
						}
				}
				]
			}
		})

		getBrokers: (#WFSteps & { name: "getBrokers"	})
		getUserConfirmation: (#WFSuspend & { name: "getUserConfirmation"	})

		idGenerator: (#WFDoNothing & {
			#inputParams: {
				proxyEndpoint: { requiredArg: false, type: string }
				serviceName:   { requiredArg: true, type: string }
			}
		})
	}
}