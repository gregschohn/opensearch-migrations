package mymodule

#MIGRATION_TEMPLATES: CAPTURE_REPLAY: #K8sWorkflowTemplate & {
  entrypoint:         "runAll"
  serviceAccountName: "argo-workflow-executor"

	#name: "captureReplay"
	_templateSignaturesMap: [string]: #TemplateSignature
	#templates: {
  	runAll: (#WFDag & {
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
  		_parsedParams: {...}

			dag: {
			  tasks: [{
						#templateSignature: _templateSignaturesMap.idGenerator
					  #argumentMappings: {
							serviceName: paramWithName: #ParameterWithName & _parsedParams.parameterMap.sessionName
							//proxyEndpoint: expression:
						}
				}
				]
			}
		})

		getBrokers: (#WFSteps & { name: "getBrokers"	})
		getUserConfirmation: (#WFSuspend & { name: "getUserConfirmation"	})

		idGenerator: (#WFDoNothing & {
			#parameters: {
				proxyEndpoint: { requiredArg: false, type: string }
				serviceName:   { requiredArg: true, type: string }
			}
		})
	}
}