package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"

#Spec: argo.#."io.argoproj.workflow.v1alpha1.WorkflowSpec" & {
		#workflowParameters: [string]: #Parameters.#WorkflowParameter
	  #workflowParameters: #WORKFLOW_PARAMS

		steps?: [...]
		_workflowParameters: { for p, details in #workflowParameters { (p): {#FullyProjectedParameter & { parameterName: p, details } } } }
		_workflowParametersList: [ for p, v in _workflowParameters { v } ]
		if _workflowParametersList != [] {
			arguments: {
				parameters: [for p in _workflowParametersList {
					name: p.parameterName,
					p._argoValue.inlineableValue,
					if (!p.requiredArg && !p._argoValue._hasDefault) { value: "" }
				}]
			}
		}
}
