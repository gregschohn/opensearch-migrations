package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"
import k8sAppsV1 "k8s.io/apis_apps_v1"

let TOP_CONTAINER = #Container

#ProxyInputsIntoArguments: {
  #in: [string]: #Parameters.#TemplateParameter
  out: [
  	for k, v in #in {
  		name: k, value: ({ (#FullyProjectedParameter & { parameterName: k, v }) }).templateInputPath
   	}
  ]
}

#WFTemplate: {
  #ParametersExpansion: {
		#parameters: [string]: #Parameters.#TemplateParameter
		_parametersList: [for p, details in #parameters { details, parameterName!: p }]
		if _parametersList != [] {
			inputs: {
				parameters: [for p in _parametersList {
					name: p.parameterName
					p._argoValue.inlineableValue,
					if (!p.requiredArg && !p._argoValue._hasDefault) { value: "" }
				}]
			}
		}
		_paramsWithTemplatePathsMap: {for k, v in #parameters {"\(k)": { #FullyProjectedParameter & { parameterName: k, v } } } }
  }

  #ArgumentsExpansion: {
  	#args: [string]: _ //#Parameters.#ValueFiller
		_parametersList: [...]
		if len(#args) > 0 {
			arguments: {
				parameters: [for a in #args {
					name: a.name,
					(#Parameters.#FillValue & { in: a }).out
				}]
			}
		}
  }

	#TemplateExpansion: {
		#templateObj: #Base
		template: #templateObj.name
		name: template
	}

  #WorkflowStepOrTask: {
  	#ArgumentsExpansion
  	#TemplateExpansion
  	...
//  	argo.#."io.argoproj.workflow.v1alpha1.DAGTask"
//  	argo.#."io.argoproj.workflow.v1alpha1.WorkflowStep"
  }

 #Base: (argo.#."io.argoproj.workflow.v1alpha1.Template" & {
		#ParametersExpansion
		steps?: [...]

		name: string
		inputs?: {...}
		outputs?: {...}
 })

 #Dag:       { #Base, dag: tasks: [...#WorkflowStepOrTask] }
 #Steps:     { #Base, steps: [...[...#WorkflowStepOrTask]] }
 #Suspend:   { #Base, suspend: {} }
 #DoNothing: { #Base, steps: [[]] }
 #Container: { #Base, container: TOP_CONTAINER.#Base } // find the argo type for this part or the parent
 #Script:    { #Base, script: //argo.#."io.argoproj.workflow.v1alpha1.ScriptTemplate" & #Container.#Base &
 	{}
 }

 #Resource: {
  #Base
  name: string
  #manifestSchema!: {...}
  #manifest: (#ManifestUnifier & { in: #manifestSchema }).out

  resource: argo.#."io.argoproj.workflow.v1alpha1.ResourceTemplate" & {
    setOwnerReference: bool // no default - too important.  Always specify.
    manifest: (#EncodeCueAsJsonText & {in: #manifest} ).out
  }
 }

 #Deployment: close({
 	#Resource
  #manifestSchema: k8sAppsV1.#SchemaMap."io.k8s.api.apps.v1.Deployment"
  #resourceName!: string
  #parameters!: [string]: #Parameters.#TemplateParameter
  #containers: [{...}]

  #manifest:
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
 })
}

