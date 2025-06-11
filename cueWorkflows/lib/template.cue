package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"
import k8sAppsV1 "k8s.io/apis_apps_v1"

let TOP_CONTAINER = #Container

#ProxyInputsIntoArguments: {
  #in: [string]: #TemplateParameterDefinition
  out: [
  	for k, v in #in {
  		name: k, value: ({ (#ParameterWithName & { parameterName: k, v }) }).templateInputPath
   	}
  ]
}

  #ParametersExpansion: {
		#parameters: [string]: (#TemplateParameterDefinition)
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
		_paramsWithTemplatePathsMap: {for k, v in #parameters {"\(k)": { #ParameterWithName & { parameterName: k, v } } } }
  }

  #ArgumentsExpansion: {
  	#args: [string]: _ //#ValueFiller
		_parametersList: [...]
		if len(#args) > 0 {
			arguments: {
				parameters: [for a in #args {
					name: a.name,
					(#FillValue & { in: a }).out
				}]
			}
		}
  }

	#TemplateExpansion: {
		#templateObj: #WFBase
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

 #WFBase: (argo.#."io.argoproj.workflow.v1alpha1.Template" & {
		#ParametersExpansion
		steps?: [...]

		name: string
		inputs?: {...}
		outputs?: {...}
 })

 #WFDag:       { #WFBase, dag: tasks: [...#WorkflowStepOrTask] }
 #WFSteps:     { #WFBase, steps: [...[...#WorkflowStepOrTask]] }
 #WFSuspend:   { #WFBase, suspend: {} }
 #WFDoNothing: { #WFBase, steps: [[]] }
 #WFContainer: { #WFBase, container: TOP_CONTAINER.#Base } // find the argo type for this part or the parent
 #WFScript:    { #WFBase, script: //argo.#."io.argoproj.workflow.v1alpha1.ScriptTemplate" & #Container.#Base &
	{}
 }

 #WFResource: {
  #WFBase
  name: string
  #manifestSchema!: {...}
  #manifest: (#ManifestUnifier & { in: #manifestSchema }).out

  resource: argo.#."io.argoproj.workflow.v1alpha1.ResourceTemplate" & {
    setOwnerReference: bool // no default - too important.  Always specify.
    manifest: (#EncodeCueAsJsonText & {in: #manifest} ).out
  }
 }

 #WFDeployment: close({
 	#WFResource
  #manifestSchema: k8sAppsV1.#SchemaMap."io.k8s.api.apps.v1.Deployment"
  #resourceName!: string
  #parameters!: [string]: #TemplateParameterDefinition
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


