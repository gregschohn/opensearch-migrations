package mymodule

import "strconv"
import "strings"

#FromParam: {
	paramWithName: { #ParameterWithName }
}

#FromConfigMap: {
	map!:   string
	key!:   string
	type!: _
}

#FromExpression: {
	e: string
}

#LiteralValue:   bool | string | number
#ComputedValue:  #FromParam | #FromConfigMap | #FromExpression
#ArgoValue: #LiteralValue | #ComputedValue

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
#ArgoValueProperties: {
	if (#in & #LiteralValue) != _|_ {
		_checkTypeIsConcrete: (#IsConcreteValue & {#value: #in}).concrete & true
	}

	#in: #ArgoValue
	// What type does the incoming value belong to
	inferredType: _parsedValue.t
  // what should be the contents dumped into the parameters object to signify this value...
  // a string, "value": "{{...parameter.name}}", "valueFrom": {...}
	parameterContents: _parsedValue.inlinedValue

	_parsedValue: ([
  	if (#in & #FromParam) != _|_ {
			t: #in.paramWithName.parameterDefinition.type,
			inlinedValue: value: #in.paramWithName.templateInputPath
		},
		if (#in & #FromConfigMap) != _|_ {
			t: #in.type,
			inlinedValue: valueFrom: configMapKeyRef: {
					name: #in.map
					key:  #in.key
				}
		},

		if (#in & bool)       != _|_ {t: bool,   inlinedValue: value: strconv.FormatBool(#in)},
		if (#in & int)        != _|_ {t: int,    inlinedValue: value: strconv.FormatInt(#in, 10)},
		if (#in & float)      != _|_ {t: float,  inlinedValue: value: strconv.FormatFloat(#in, strings.Runes("f")[0], -1, 32)},
		if (#in & string)     != _|_ {t: string, inlinedValue: value: #in},

		if (#in & [...])      != _|_ {t: [...], inlinedValue: value: "FORMAT THIS List AS JSON"},
		if (#in & {...})      != _|_ {t: {...}, inlinedValue: value: "FORMAT THIS Struct AS JSON"},

		{t: _, inlinedValue: value: "badstuff"}
	][0])
}

#BaseParameterDefinition: {
	parameterSource: "inputs"| "workflow"
	defaultValue?:   #ArgoValue
  description?: string
	requiredArg:  *false | bool

	_hasDefault: (defaultValue & _) != _|_
	if (_hasDefault) {
		type: (#ArgoValueProperties & { #in: defaultValue }).inferredType
		parameterContents: (#ArgoValueProperties & { #in: defaultValue }).parameterContents
	}
	if (!_hasDefault) {
		type: _
		if (!requiredArg) {
			parameterContents: value: ""
		}
		if (requiredArg) { parameterContents: {} }
	}
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
	parameterDefinition: (#TemplateParameterDefinition | #WorkflowParameterDefinition)
	parameterName!:    string
	envName:           string | *strings.ToUpper(parameterName)
	parameterPath:     "\(parameterDefinition.parameterSource).parameters['\(parameterName)']"
	templateInputPath: "{{\(parameterPath)}}"
}
