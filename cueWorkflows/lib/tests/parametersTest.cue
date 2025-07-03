Test:{
	Params: {
    One: {
			  _p: {#ParameterWithName & {parameterName: "p", parameterDefinition: #TemplateParameterDefinition & {type: int } } }
				pIsNotConcrete: ((#IsConcreteValue & { #value: _p.parameterDefinition.type }).concrete) == false
				av1: #ArgoParameterValueProperties & { #in: { paramWithName: _p } }
				av2: #ArgoParameterValueProperties & { #in: "string" }
    },
		Two: {
				a: #ParameterWithName & { parameterDefinition: { parameterSource: "workflow", defaultValue: "nine" }, parameterName: "a" }
				// WorkflowParameterDefinition will unify to parameterSource: "workflow"
				a: #ParameterWithName & { parameterDefinition: #WorkflowParameterDefinition & { defaultValue: "nine" }, parameterName: "a" }
				b: #ParameterWithName & {
				  parameterName: "b",
				  parameterDefinition: {
				    parameterSource: "inputs"
				    defaultValue: { #FromParam & {paramWithName: a} }
				  }
				}
		}
	}
}