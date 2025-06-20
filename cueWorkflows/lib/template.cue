package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"
import k8sAppsV1 "k8s.io/apis_apps_v1"

let TOP_CONTAINER = #Container

#ProxyInputsIntoArguments: {
  #in: [string]: #TemplateParameterDefinition
  out: [
  	for k, v in #in {
  		name: k, value: ({ (#ParameterWithName & { parameterName: k, parameterDefinition: v }) }).templateInputPath
   	}
  ]
}

#ParametersExpansion: {
	#in: {...}
	_parametersList: [for p, details in #in { parameterDefinition: details, parameterName!: p }]
	inputs: {
		parameters: [for p in _parametersList {
			name: p.parameterName
			p.parameterDefinition.parameterContents
		}]
	}
	_paramsWithTemplatePathsMap: {
		for k, v in #in {"\(k)": { #ParameterWithName & { parameterName: k, parameterDefinition: v } } }
	}
}

//#ArgumentsExpansion: {
// 	#args: [string]: _ //#ValueFiller
//	_parametersList: [...]
//	if len(#args) > 0 {
//		arguments: {
//			parameters: [for a in #args {
//				name: a.name,
//				(#FillValue & { in: a }).out
//			}]
//		}
//	}
//}

#WorkflowStepOrTask: {
	//#ArgumentsExpansion
	#templateObj: #WFBase
	template: #templateObj.name
	name: template
  ...
//  	argo.#."io.argoproj.workflow.v1alpha1.DAGTask"
//  	argo.#."io.argoproj.workflow.v1alpha1.WorkflowStep"
  }

#WFBase: (argo.#."io.argoproj.workflow.v1alpha1.Template" & {
	#parameters: [string]: (#TemplateParameterDefinition)
	#args: [string]: _

	if len(#parameters) != 0 {
		_expandedParameters: (#ParametersExpansion) & { #in: #parameters }
		inputs: _expandedParameters.inputs
	}
	name: string
	steps?: [...]
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

  #manifest: {
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


