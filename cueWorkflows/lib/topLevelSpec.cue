package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"

#Spec: argo.#."io.argoproj.workflow.v1alpha1.WorkflowSpec" & {
		#workflowParameters: [string]: #WorkflowParameterDefinition
	  #workflowParameters: #WORKFLOW_PARAMS

		steps?: [...]
		_workflowParameters: {
			for p, details in #workflowParameters {
				(p): {#ParameterWithName & { parameterName: p, parameterDefinition: details } }
			}
		}
		_workflowParametersList: [ for p, v in _workflowParameters { v } ]
		if (_workflowParametersList & []) == _|_ {
			arguments: {
				parameters: [for p in _workflowParametersList {
					name: p.parameterName,
					(#ValuePropertiesFromParameter & { #parameterDefinition: p.parameterDefinition }).parameterContents
				}]
			}
		}
}
