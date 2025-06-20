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
					#parameters: {
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
					#parameters: {
							image:    { passToContainer: false, defaultValue: "migrations/traffic_replayer:latest" }
							replicas: { passToContainer: false, type: int }

							requiredParam: { requiredArg: true, type: string }

							defIntParam:  defaultValue: 70
							defNumParam:  defaultValue: 1.1
							defBoolParam: defaultValue: false
							defStrParam:  defaultValue: "testString"

							typeBool: type: bool
							typeStr:  type: string
							typeNum:  type: string
					}
				}
				_assertUnify: close(obj) & close({
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
					#parameters: {
						p: defaultValue: {
							map: "testConfigMapName"
							key: "testKey"
							type: string
						}
					}
				}
				_assertUnify: obj & close({
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
					_expandedParameters: {...}
					#parameters: {
						baseParam: defaultValue: "baseValue"
						p: defaultValue: {
							paramWithName: _expandedParameters._paramsWithTemplatePathsMap["baseParam"]
						}
					}
				}
				_assertUnify: obj & {
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
		}
		StepsTemplate: {

			innerTemplate: #WFDoNothing & {
				name: "dummy"
			}

			obj: #WFSteps & {
				name: "test"
				#parameters: {
					strParam: type: string
					intParam: type: int
				}

				steps: [[{
					#templateObj: innerTemplate
					#args: {
						to: innerTemplate._expandedParameters._paramsWithTemplatePathsMap['dummy2'],
					  map: "testConfigMap",
					  key: "testKey",
					  type2: string
					 }
				}]]
			}

		}
	}
}
