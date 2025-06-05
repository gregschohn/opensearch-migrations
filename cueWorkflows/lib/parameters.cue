package mymodule

import "strconv"
import "strings"

#Parameters: {

	#FromWorkflowParam: {
		fromW: string
	}

	#FromTemplateParam: {
		fromT: string
	}

	#FromConfigMap: {
		map!: string
		key!: string
		#type!: _
	}

	#FromExpression: {
		e: string
	}

	#FillValue: {
		in: #ComputedValue
		out: [
			if (in & #FromWorkflowParam) != _|_ {
			},
//			if (in & #FromTemplateParam) != _|_ {},
			if (in & #FromConfigMap) != _|_ {
				valueFrom: configMapKeyRef: {
				  name: in.map
				  key: in.key
				}
			},
//			if (in & #FromExpression) != _|_ {},
			in
		][0]
	}

	#LiteralValue: bool | string | number
	#ComputedValue: #FromWorkflowParam | #FromTemplateParam | #FromConfigMap | #FromExpression

	#ArgoValue: {
	  value: #LiteralValue | #ComputedValue | *null
	  _typeIsConcrete: (#IsConcreteValue & { #value: type }).concrete & false,
		_hasDefault: (value & null) == _|_,
		//_checkDefaultType: (type & value),
		type: _projectedvalue.t,

		_projectedvalue: ({t: _, v: _, f: _}) & ([
			if (value & null) != _|_   { t: _,   inlined: {} },
			if (value & #LiteralValue) != _|_ {
  	  	{
  	  		let typeAndVal = [
							if (value & bool) != _|_   { t: bool,   v: strconv.FormatBool(value) },
							if (value & int) != _|_    { t: int,    v: strconv.FormatInt(value, 10) },
							if (value & float) != _|_  { t: float,  v: strconv.FormatFloat(value, strings.Runes("f")[0], -1, 32) },
							if (value & string) != _|_ { t: string, v: value },
							if (value & [...]) != _|_  { t: [...],  v: "FORMAT THIS List AS JSON" },
							if (value & {...}) != _|_  { t: {...},  v: "FORMAT THIS Struct AS JSON" },
							{ t: _ }
					][0]
					{ t: typeAndVal.t, inlined: value: typeAndVal.v }
				}
			},
		  if (value & #ComputedValue) != _|_ {
		  	[
		  		if (value & #FromWorkflowParam) != _|_ {
		  			t: null, inlined: {}
		  		},
		  		if (value & #FromConfigMap) != _|_ {
		  			t: value.#type, inlined: (#FillValue & { in: value }).out
		  		},
    		][0]},
   		{ t: _, inlined: {} }
		][0]),
		inlineableValue: _projectedvalue.inlined
	}

  #BaseParameter: {
  	_argoValue: #ArgoValue,
  	_argoValue: value: defaultValue,
  	_argoValue: type: type,

		parameterSource: string
  	defaultValue: _argoValue.value,
  	type: _argoValue.type,

		description?:  string,
    requiredArg: *false | bool,
  }

	#TemplateParameter: {
		#BaseParameter,
		parameterSource: "inputs",
		passToContainer: *true | bool,
		requiredArg: bool

		_checkRequiredAndPassAgreement: {
				if (passToContainer != true) { check: false & requiredArg }
				if (requiredArg == true)     { check: true & passToContainer }
		}
	}

	#WorkflowParameter: {
		#BaseParameter,
		parameterSource: "workflow"
	}

	//#: "io.argoproj.workflow.v1alpha1.ValueFrom"
	#ArgumentParameter: "io.argoproj.workflow.v1alpha1.Parameter"
}
