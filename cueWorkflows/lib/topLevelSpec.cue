package mymodule

//import argo "github.com/opensearch-migrations/workflowconfigs/argo"

#Spec: {
	...
}
//argo.#."io.argoproj.workflow.v1alpha1.WorkflowSpec" & {
//		#workflowParameters: [string]: #Parameters.#WorkflowParameter
//		steps?: [...]
//		_parametersList: [for p, details in #workflowParameters { details & #ParameterAndInputPath & { parameterName: p }}]
//		if _parametersList != [] {
//			arguments: {
//				parameters: [for p in _parametersList {
//					name: p.parameterName
//					if p.defaultValue != _|_ { value: p._defaultValueAsStr }
//					if (!p.requiredArg && !p._hasDefault) { value: "" }
//				}]
//			}
//		}
//}
