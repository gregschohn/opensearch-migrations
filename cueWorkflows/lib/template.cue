package mymodule

import (
	"encoding/json"
	argo "github.com/opensearch-migrations/workflowconfigs/argo"
)

#ParameterAndInputPath: #ParameterDetails & {
	parameterName:     string
	exprInputPath:     "inputs.parameters['\(parameterName)']"
	templateInputPath: "{{\(exprInputPath)}}"
}

#ProxyInputsIntoArguments: {
	#in: [string]: #ParameterDetails
	out: [for k, v in #in let u = #ParameterAndInputPath & v {name: u.parameterName, value: u.templateInputPath}]
}

#ArgoTemplate:         argo.#."io.argoproj.workflow.v1alpha1.Template"
#ArgoResourceTemplate: argo.#."io.argoproj.workflow.v1alpha1.ResourceTemplate"

#Template: #ArgoTemplate &
	{
		#parameters: [string]: #ParameterDetails
		steps?: [...]
		_parametersList: [for p, details in #parameters {details & #ParameterAndEnvironmentName & {parameterName: p}}]
		if _parametersList != [] {
			inputs: parameters:
			[for p in _parametersList {
				{
					name: p.parameterName
				}
				if p.defaultValue != _|_ {
					value: p.defaultValue
				}
			}]
		}
		_paramsWithTemplatePathsMap: {for k, v in #parameters {"\(k)": {#ParameterAndInputPath & v & {parameterName: k}}}}

		name: string
		inputs?: {...}
		outputs?: {...}
	}

#ResourceTemplate: #Template & {
	name:      string
	#manifest: _
	resource: #ArgoResourceTemplate & {

		setOwnerReference: bool // no default - too important.  Always specify.
		manifest:          json.Marshal(#manifest)
		...
	}
}

#StepTemplate: close(#Template & {
	steps: [...]
})
