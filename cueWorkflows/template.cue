package mymodule

#ParameterAndInputPath: #ParameterDetails & {
    parameterName:  string
    templateInputPath: "{{inputs.parameters['\(parameterName)']}}"
}

#ProxyInputsIntoArguments: {
	#in: [string]: #ParameterDetails
	out: [ for k,v in #in let u=#ParameterAndInputPath & v { name: u.parameterName, value: u.templateInputPath}]
}

#Template: {
	#parameters: [string]: #ParameterDetails
	_parametersList: [ for p, details in #parameters { details & #ParameterAndEnvironmentName & { parameterName:  p } } ]
    if _parametersList != [] {
        inputs:
            parameters:
                [for p in _parametersList {
                    {
                        name: p.parameterName
                    }
                    if p.defaultValue != _|_ {
                        value: p.defaultValue
                    }
                }]
    }
		_paramsWithTemplatePathsMap: { for k,v in #parameters { "\(k)": {#ParameterAndInputPath & v & {parameterName: k} }}}

		name: string
		inputs?: {...}
    outputs?: {...}
    ...
}

#ResourceTemplate: #Template & {
    name: string
    resource: {...
        action: "create"
        setOwnerReference: bool // no default - too important.  Always specify.
        manifest: string
    }
}

#StepTemplate: #Template & { ...
	steps: [...]
}
