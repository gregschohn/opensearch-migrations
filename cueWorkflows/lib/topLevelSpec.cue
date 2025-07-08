package mymodule

import argo "opensearch.org/migrations/workflowconfigs/argo"

#K8sWorkflowTemplate: argo.#."io.argoproj.workflow.v1alpha1.WorkflowTemplate" & {
	#name!: string
	#inputParams: #WORKFLOW_PARAM_SET,
	#templates: [string]: #WFBase

	apiVersion:     "argoproj.io/v1alpha1"
	kind:           "WorkflowTemplate"
	metadata: name: #name

  spec: {
		_workflowParameters: (#ParametersExpansion & { #in: #inputParams })
		arguments: parameters: _workflowParameters.parameters
		templates: _templateList
	}

	_templateList: [...]
//	#in: #K8sWorkflowTemplate,
	if (len(#templates) > 0) {
		_templateSignaturesMap: { for k, v in #templates {
			"\(k)": #TemplateSignature & { name: k, parameters: v.#inputParams, containingKubernetesResourceName: #name }
	   } }
		_templateList: [ for k, v in #templates { name: "\(k)", v } ]
	}
}
