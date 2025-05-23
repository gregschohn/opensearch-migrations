package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"
import k8sAppsV1 "k8s.io/apis_apps_v1"

#ParameterAndInputPath: #ParameterDetails & {
  parameterName!:    string
  exprInputPath:     "inputs.parameters['\(parameterName)']"
  templateInputPath: "{{\(exprInputPath)}}"
}

#ProxyInputsIntoArguments: {
  #in: [string]: #ParameterDetails
  out: [for k, v in #in let u = #ParameterAndInputPath & v {name: u.parameterName, value: u.templateInputPath}]
}

#WFTemplate: {
 Base: argo.#."io.argoproj.workflow.v1alpha1.Template" & {
		#parameters: [string]: #ParameterDetails
		steps?: [...]
		_parametersList: [for p, details in #parameters { details & #ParameterAndEnvironmentName & {parameterName: p}}]
		if _parametersList != [] {
			inputs: {
				parameters: [for p in _parametersList {
					name: p.parameterName
					if p.defaultValue != _|_ { value: p._defaultValueAsStr }
					if (!p.requiredArg && !p._hasDefault) { value: "" }
				}]
			}
		}
		_paramsWithTemplatePathsMap: {for k, v in #parameters {"\(k)": { #ParameterAndInputPath & v & {parameterName: k} } } }

		name: string
		inputs?: {...}
		outputs?: {...}
 }

 Dag: Base & {
  dag: tasks: [...]
 }

 Steps: Base & {
  steps: [...]
 }

 Suspend: Base & {
  suspend: {}
 }

 DoNothing: Base & {
  steps: [[]]
 }

 Resource: Base & {
  name: string
  #manifest: {...}
  resource: argo.#."io.argoproj.workflow.v1alpha1.ResourceTemplate" & {
    setOwnerReference: bool // no default - too important.  Always specify.
    manifest: (#EncodeCueAsJsonText & {in: #manifest} ).out
    ...
  }
 }

 Deployment: #WFTemplate.Resource & {
  #resourceName!: string
  #parameters!: [string]: #ParameterDetails
  #containers: [{...}]

  #manifest: (#ManifestUnifier & {in: k8sAppsV1.#SchemaMap."io.k8s.api.apps.v1.Deployment"}).out &
    {
      apiVersion: "apps/v1"
      kind:       "Deployment"
      metadata: {
        generateName: "\(#resourceName)-"
        labels: app: #resourceName
      }

      spec: {
        replicas!: _
        selector: matchLabels: app: #resourceName
        template: {
          metadata: labels: app: #resourceName
          spec: containers: #containers
        }
      }
    }
 }
}
