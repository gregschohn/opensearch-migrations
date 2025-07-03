package mymodule

import argo "github.com/opensearch-migrations/workflowconfigs/argo"
import k8sAppsV1 "k8s.io/apis_apps_v1"
import "strings"
import "strconv"

@experiment(structcmp)

let TOP_CONTAINER = #Container

#ParametersExpansion: {
	// constrain that all parameterSource values must be the same
	#in: [string]: { ..., parameterSource: _parameterSource }
	_parameterSource: string

	let PARAMS_LIST = [for p, details in #in { parameterDefinition: #BaseParameterDefinition & details, parameterName!: p }]
	parameters: [for p in PARAMS_LIST {
		name: p.parameterName,
		(#ValuePropertiesFromParameter & { #parameterDefinition: p.parameterDefinition }).parameterContents,
	}]

	parameterMap: {
		for k, v in #in {"\(k)": { #ParameterWithName & {..., parameterName: k, parameterDefinition: v } } }
	}
}

#WorkflowStepOrTask: {...
//  	argo.#."io.argoproj.workflow.v1alpha1.DAGTask"
//  	argo.#."io.argoproj.workflow.v1alpha1.WorkflowStep"
	#templateSignature!: #TemplateSignature,
	#argumentMappings: [string]: #ArgoParameterValue

	templateRef: #templateSignature.templateRef,
	name: "\(#templateSignature.containingKubernetesResourceName)--\(#templateSignature.name)",

  // The balance of this struct handles arguments and their agreement with the target
	let TARGET_PARAMS = #templateSignature.parameters | *error("Cannot find params for \(#templateSignature.name) template"),
	if len(#argumentMappings) > 0 {
		arguments: {
			parameters: [for k, v in #argumentMappings {
				name: k,
				let AVP = #ArgoParameterValueProperties & { #in: v}
				AVP.parameterContents,
				_parameterExists: (TARGET_PARAMS[k] & {...}) | *error("Could not find parameter \(k)")
				_typesAgree: (AVP.inferredType & TARGET_PARAMS[k].type) | *error("The argument and parameter types don't agree for \(k)")
			}]
		}
	}
	let MISSING_REQUIRED_ARGS = [ for k,v in TARGET_PARAMS if (v.requiredArg && #argumentMappings[k] == _|_) { k } ]
	_check: (len(MISSING_REQUIRED_ARGS) & 0) |
	    *error("Missing (\(strconv.FormatInt(len(MISSING_REQUIRED_ARGS),10))) required arguments to \(#templateSignature.name): \(strings.Join(MISSING_REQUIRED_ARGS, ", "))")
}

#TemplateSignature: {
	let N = name,
	name: string,
	parameters: {...},
	containingKubernetesResourceName!: string
	templateRef: {
		name:     N
		template: containingKubernetesResourceName
	}
}

#GetTemplateSignature: {
	#template: #WFBase,
	#containingKubernetesResourceName!: string
	out: {
		name: #template.name,
		parameters: #template.#parameters,
		containingKubernetesResourceName: #containingKubernetesResourceName
	}
}

#WFBase: (argo.#."io.argoproj.workflow.v1alpha1.Template" & {
	#parameters: [string]: (#TemplateParameterDefinition)

	if len(#parameters) != 0 {
		_parsedParams: (#ParametersExpansion & { #in: #parameters })
		inputs: parameters: _parsedParams.parameters
	}
	name!: string
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
  outputs?: {
  	for i, param in outputs.parameters {
  		if param.valueFrom.jsonPath == _|_ && param.valueFrom.jqFilter == _|_ {
  			"_error_\(i)": error("Resource \(name) parameter \(param.name) must use jsonPath or jqFilter")
			}
		}
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


