package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"
import k8sAppsV1 "k8s.io/apis_apps_v1"
import "strings"
import "strconv"

let TOP_CONTAINER = #Container

#ProxyInputsIntoArguments: {
  #in: {...}
  out: [
  	for k, v in #in {
  		name: k,
  		value: v.templateInputPath
   	}
  ]
}

#ParametersExpansion: {
	#in: {...}
	let PARAMS_LIST = [for p, details in #in { parameterDefinition: #TemplateParameterDefinition & details, parameterName!: p }]
	inputs: {
		parameters: [for p in PARAMS_LIST {
			name: p.parameterName,
			(#ValuePropertiesFromParameter & { #parameterDefinition: p.parameterDefinition }).parameterContents
		}]
	}
	_parameterMap: {
		for k, v in #in {"\(k)": { #ParameterWithName & {..., parameterName: k, parameterDefinition: v } } }
	}
}

#WorkflowStepOrTask: {...
//  	argo.#."io.argoproj.workflow.v1alpha1.DAGTask"
//  	argo.#."io.argoproj.workflow.v1alpha1.WorkflowStep"
	#templateObj: #WFBase
	#argMappings: [string]: #ArgoValue

	template: #templateObj.name
	name: template

  // The balance of this struct handles arguments and their agreement with the target
	let TARGET_PARAMS = #templateObj.#parameters | *error("Cannot find params for \(#templateObj.name) template")
	if len(#argMappings) > 0 {
		arguments: {
			parameters: [for k, v in #argMappings {
				name: k,
				let AVP = #ArgoValueProperties & { #in: v}
				AVP.parameterContents,
				_parameterExists: (TARGET_PARAMS[k] & {...}) | *error("Could not find parameter \(k)")
				_typesAgree: (AVP.inferredType & TARGET_PARAMS[k].type) | *error("The argument and parameter types don't agree for \(k)")
			}]
		}
	}
	let MISSING_REQUIRED_ARGS = [ for k,v in TARGET_PARAMS if (#argMappings[k] == _|_) { k } ]
	_check: (len(MISSING_REQUIRED_ARGS) & 0) |
	  *error("Missing (\(strconv.FormatInt(len(MISSING_REQUIRED_ARGS),10))) required arguments to \(#templateObj.name): \(strings.Join(MISSING_REQUIRED_ARGS, ", "))")
}

#WFBase: (argo.#."io.argoproj.workflow.v1alpha1.Template" & {
	#parameters: [string]: (#TemplateParameterDefinition)
	#args: [string]: _

	if len(#parameters) != 0 {
		_parsedParams: (#ParametersExpansion & { #in: #parameters })
		inputs: _parsedParams.inputs
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


