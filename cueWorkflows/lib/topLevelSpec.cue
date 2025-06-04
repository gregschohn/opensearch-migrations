package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"

#Spec: argo.#."io.argoproj.workflow.v1alpha1.WorkflowSpec" & {
		#workflowParameters: [string]: #Parameters.#WorkflowParameter
		steps?: [...]
		_workflowParameters: { for p, details in #workflowParameters { (p): {#ParameterAndInputPath, details, parameterName: p } } }
		_workflowParametersList: [ for p, k in _workflowParameters { k }]
		if _workflowParametersList != [] {
			arguments: {
				parameters: [for p in _workflowParametersList {
					name: p.parameterName
					p._argoValue.inlineableValue,
					if (!p.requiredArg && !p._argoValue._hasDefault) { value: "" }
				}]
			}
		}
}
