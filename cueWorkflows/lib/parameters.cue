package mymodule

import "strconv"
import "strings"

#Parameters: {
	ParameterType: {
  	type!:         "bool" | "string" | "int" | "float"
	  defaultValue?: bool | string | number // Argo limits this to be only a string, so we'll have to encode
		description?:  string

		_hasDefault: (defaultValue != _|_)
		_checkDefaultType: (#cueType & defaultValue)

		_defaultValueAsStr: ([
				if (defaultValue != null && type == "int") { strconv.FormatInt(defaultValue, 10) },
				if (defaultValue != null && type == "bool") { strconv.FormatBool(defaultValue) },
				if (defaultValue != null && type == "float") { strconv.FormatFloat(defaultValue, strings.Runes("f")[0], -1, 32) },
				if (defaultValue != null && type == "string") { defaultValue },
				null
		][0])
		#cueType: ([
				if (type == "int")    { int },
				if (type == "bool")   { bool },
				if (type == "float")  { float },
				if (type == "string") { string }
		][0])
	}

	ParameterDetails: ParameterType & {
    requiredArg: *false | bool
		passToContainer: *true | bool

		_checkRequiredAndPassAgreement: {
				if (passToContainer != true) { check: false & requiredArg }
				if (requiredArg == true)     { check: true & passToContainer }
		}
	  ...
	}

	WorkflowDetails: ParameterType
}
