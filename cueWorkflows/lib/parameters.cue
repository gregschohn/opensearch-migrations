package mymodule

import "strconv"
import "strings"

#FromParam: {
	paramWithName: { #ParameterWithName }
}

#FromConfigMap: {
	map!:   string & #noBracesString | #FromParam
	key!:   string & #noBracesString | #FromParam
	type!: _
}

#FromExpression: {
	expression: #Expression
}

#ArgoTemplateString: {
	argoReadyString: string
}

#LiteralValue:   bool | string | number
#ArgoExpressionValue: #LiteralValue | #FromParam | #FromExpression | #ArgoTemplateString
#ArgoParameterValue:  #LiteralValue | #FromParam | #FromExpression | #ArgoTemplateString | #FromConfigMap

// Every defined parameter can have a value, which this represents.
// For our enhanced schema, values may be types beyond just strings.
// They can be numbers, booleans, compound datatypes, or strings.
// This object represents _some_ runtime object, maybe literal values, like 1, "value".
// The value can also represent a reference to another value, a configmap, or an expression,
// which are all natively supported by Argo.
//
// We want to model a bit more than just what Argo supports so that we can 1) check that the
// usage of a value will be consistent with the context that it's being used within (e.g. replicas
// should represent an integer).  2) We want to make sure that we're referring to values that are
// defined elsewhere, rather than letting a bad identifier go unnoticed.  3) This can minimize the
// amount of space that it takes to specify a parameter.
#ArgoExpressionValueProperties: {
	if (#in & #LiteralValue) != _|_ {
		_checkTypeIsConcrete: (#IsConcreteValue & {#value: #in}).concrete & true
	}

	#in: #ArgoExpressionValue
	inferredType: _parsedValue.t	// What type does the incoming value belong to
	value: _parsedValue.value

	_parsedValue: ([
//  	if (#in & #Expression) != _|_ {
//			t: #in.expression,
//			value: #in.paramWithName.templateInputPath
//		},
  	if (#in & #FromParam) != _|_ {
			t: #in.paramWithName.parameterDefinition.type,
			value: #in.paramWithName.templateInputPath
		},
		if (#in & #ArgoTemplateString) != _|_ {
			t: _,
			value: #in.argoReadyString
		},
		if (#in & bool)       != _|_ {t: bool,   value: strconv.FormatBool(#in)},
		if (#in & int)        != _|_ {t: int,    value: strconv.FormatInt(#in, 10)},
		if (#in & float)      != _|_ {t: float,  value: strconv.FormatFloat(#in, strings.Runes("f")[0], -1, 32)},
		if (#in & string)     != _|_ {t: string, value: #in},
//		{ t: _, value: UNKNOWN: #in }
	][0])
}
#ArgoParameterValueProperties: {
	#in: #ArgoParameterValue
	let IN_PARAM_VALUE = #in
	// What type does the incoming value belong to
	inferredType: _parsedValue.t
  // what should be the contents dumped into the parameters object to signify this value...
  // a string, "value": "{{...parameter.name}}", "valueFrom": {...}
	parameterContents: _parsedValue.inlinedValue

	_parsedValue: ([
		if (#in & #FromConfigMap) != _|_ {
			t: #in.type,
			inlinedValue: valueFrom: configMapKeyRef: {
					if (#in.key & #FromParam) != _|_ {
						key: #in.key.paramWithName.templateInputPath
					}
					if (#in.key & string) != _|_ {
						key:  #in.key
					}
					if (#in.map & #FromParam) != _|_ {
						name: #in.map.paramWithName.templateInputPath
					}
					if (#in.map & string) != _|_ {
						name: #in.map
					}
				}
		},
		{
			let p = #ArgoExpressionValueProperties & { #in: IN_PARAM_VALUE }
			t: p.inferredType
			if (#in & #FromExpression) == _|_ {
				inlinedValue: value: p.value
			}
			if (#in & #FromExpression) != _|_ {
				inlinedValue: valueFrom: expression: p.value
			}
		}
//		{ t: _, inlinedValue: value: UNKNOWN: #in }
	][0])
}

#ValuePropertiesFromParameter: {
	#parameterDefinition: #BaseParameterDefinition
	if (#parameterDefinition._hasDefault) {
		(#ArgoParameterValueProperties & { #in: #parameterDefinition.defaultValue })
	}
	if (!#parameterDefinition._hasDefault) {
		type: #parameterDefinition.type
		parameterContents: {}
	}
}

#BaseParameterDefinition: {
	parameterSource: "inputs"| "workflow"
	defaultValue?:   #ArgoParameterValue
  description?: string
	requiredArg:  *false | bool
	type: _

	_hasDefault: defaultValue != _|_
}

#TemplateParameterDefinition: {
	#BaseParameterDefinition
	parameterSource: "inputs"
	passToContainer: *true | bool
	requiredArg:     bool

	_checkRequiredAndPassAgreement: {
		if (passToContainer != true) {check: false & requiredArg}
		if (requiredArg == true) {check: true & passToContainer}
	}
}

#WorkflowParameterDefinition: {
	#BaseParameterDefinition
	parameterSource: "workflow"
}

//_: "io.argoproj.workflow.v1alpha1.ValueFrom"
_ArgumentParameter: "io.argoproj.workflow.v1alpha1.Parameter"

#ParameterWithName: {
	// this looks weird, but it forces the parameterDefinition.parameterSource to be fully realized
	// so that the interpolation below works correctly
	parameterDefinition: (#TemplateParameterDefinition | #WorkflowParameterDefinition) & {}
	parameterName!:    string
	envName:           string | *strings.ToUpper(parameterName)
	parameterPath:     "\(parameterDefinition.parameterSource).parameters['\(parameterName)']"
	templateInputPath: "{{\(parameterPath)}}"
}
