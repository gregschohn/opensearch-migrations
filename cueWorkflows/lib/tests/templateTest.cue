Test: {
	Templates: {
		DoNothingTemplate: {
		  doNothingNoParams: #WFDoNothing & {
				name: "id-generator"
			}
		},
		InputParameters: {
			Simple: {
				obj: #WFDoNothing & {
					name: "emptyTemplateName"
					#inputParams: {
						p: type: string
					}
				}
				_assertUnify: close(obj) & {
					inputs: {
						parameters: [{
							name:  "p"
									value: ""
							}]
					}
				}
			},

		  AllKindsOfLiterals: {
		  	obj: #WFBase & {
					name: "allKindsOfLiterals"
					#inputParams: {
							image:    { passToContainer: false, parameterValue: "migrations/traffic_replayer:latest" }
							replicas: { passToContainer: false, type: int }

							requiredParam: { requiredArg: true, type: string }

							defIntParam:  parameterValue: 70
							defNumParam:  parameterValue: 1.1
							defBoolParam: parameterValue: false
							defStrParam:  parameterValue: "testString"

							typeBool: type: bool
							typeStr:  type: string
							typeNum:  type: string
					}
				}
				_assertUnify: close(obj) & ({
					name: "allKindsOfLiterals"
					inputs: {
							parameters: [{
									name:  "image"
									value: "migrations/traffic_replayer:latest"
							}, {
									name:  "replicas"
									value: ""
							}, {
									name: "requiredParam"
							}, {
									name:  "defIntParam"
									value: "70"
							}, {
									name:  "defNumParam"
									value: "1.1"
							}, {
									name:  "defBoolParam"
									value: "false"
							}, {
									name:  "defStrParam"
									value: "testString"
							}, {
									name:  "typeBool"
									value: ""
							}, {
									name:  "typeStr"
									value: ""
							}, {
									name:  "typeNum"
									value: ""
							}]
					}
				})
			}

			FromConfigMap: {
				obj: #WFDoNothing & {
					name: "emptyTemplateName"
					#inputParams: {
						p: parameterValue: {
							map: "testConfigMapName"
							key: "testKey"
							type: string
						}
					}
				}
				_assertUnify: close(obj) & ({
            name: "emptyTemplateName"
            steps: [[]]
            inputs: {
                parameters: [{
                    name: "p"
                    valueFrom: {
                        configMapKeyRef: {
                            key:  "testKey"
                            name: "testConfigMapName"
                        }
                    }
                }]
            }
        })
			}

			FromParameter: {
				obj: #WFDoNothing & {
					name: "emptyTemplateName"
					_parsedInputParams: { ... }
					#inputParams: {
						baseParam: parameterValue: "baseValue"
						p: {
							parameterValue: {
							 paramWithName: #ParameterWithName & _parsedInputParams._parameterMap["baseParam"]
							}
						}
					}
				}
				_assertUnify: close(obj) & {...
	        inputs: {
  	          parameters: [{
      	          name:  "baseParam"
    	            value: "baseValue"
        	    }, {
          	      name:  "p"
            	    value: "{{inputs.parameters['baseParam']}}"
            	}]
        	}
        }
			}

			FromConfigMapWithParam: {
				obj: #WFDoNothing & {
					name: "emptyTemplateName"
					_parsedInputParams: { ... }
					#inputParams: {
						rootMap: type: string
						rootKey: type: string
						p: parameterValue: {
							map: paramWithName: #ParameterWithName & _parsedInputParams._parameterMap["rootMap"]
							key: paramWithName: #ParameterWithName & _parsedInputParams._parameterMap["rootKey"]
							type: string
						}
					}
				}
				_assertUnify: close(obj) & ({
            name: "emptyTemplateName"
            steps: [[]]
            inputs: {
                parameters: [{
                	name: "rootMap"
                },{
                	name: "rootKey"
                },
                {
                    name: "p"
                    valueFrom: {
                        configMapKeyRef: {
                            name:  "{{inputs.parameters['rootMap']}}"
                            key: "{{inputs.parameters['rootKey']}}"
                        }
                    }
                }]
            }
        })
			}

			FromArgoReadyLiteral: {
				obj: #WFDoNothing & {
					name: "test"
					#inputParams: {
						argoSubs: parameterValue: argoReadyString: "{{workflow.uid}}"
					}
				},
				_assertUnify: close(obj) & close({
					inputs: {
						parameters: [close({
								name:  "argoSubs"
								value: "{{workflow.uid}}"
						})]
					}
					steps: _
					name: _
				})
			}
		}

		StepsTemplate: {

			innerTemplate: #WFDoNothing & {
				name: "dummy"
			}

			obj: #WFSteps & {
				name: "test"
				#inputParams: {
					strParam: type: string
					intParam: type: int
				}

				steps: [[{
					#templateSignature: (#GetTemplateSignature & { #template: innerTemplate, #containingKubernetesResourceName: "local" }).out
					#argumentMappings: {
//						#toParameter: innerTemplate.parameterMap['dummy2'],
//					  map: "testConfigMap",
//					  key: "testKey",
//					  type2: string
					 }
				}]]
			}

		}
	}
}
