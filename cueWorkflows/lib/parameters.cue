package mymodule

import "strconv"
import "strings"

#Parameters: {
	#ParameterType: {
	  defaultValue?: bool | string | number // Argo limits this to be only a string, so we'll have to encode
		description?:  string

    _typeIsConcrete: (#IsConcreteValue & { #value: type }).concrete & false
		_hasDefault: (defaultValue != _|_)
		_checkDefaultType: (type & defaultValue)
		type: _projectedDefaultValue.t

		_projectedDefaultValue: ([
	  		if (defaultValue & bool) != _|_   { t: bool, v: strconv.FormatBool(defaultValue) },
				if (defaultValue & int) != _|_    { t: int, v: strconv.FormatInt(defaultValue, 10) },
				if (defaultValue & float) != _|_  { t: float, v: strconv.FormatFloat(defaultValue, strings.Runes("f")[0], -1, 32) },
				if (defaultValue & string) != _|_ { t: string, v: defaultValue },
				if (defaultValue & null) != _|_   { t: null, v: "null" },
				if (defaultValue & [...]) != _|_  { t: [...], v: "FORMAT THIS List AS JSON" },
				if (defaultValue & {...}) != _|_  { t: {...}, v: "FORMAT THIS Struct AS JSON" },
				{ t: _, v: null }
		][0])
		_defaultValueAsStr: _projectedDefaultValue.v
	}

	#TemplateParameter: {
		#ParameterType
    requiredArg: *false | bool
		passToContainer: *true | bool

		_checkRequiredAndPassAgreement: {
				if (passToContainer != true) { check: false & requiredArg }
				if (requiredArg == true)     { check: true & passToContainer }
		}
	}

	#WorkflowParameter: {
		#ParameterType
	}

	//#: "io.argoproj.workflow.v1alpha1.ValueFrom"
	#ArgumentParameter: "io.argoproj.workflow.v1alpha1.Parameter"
}
