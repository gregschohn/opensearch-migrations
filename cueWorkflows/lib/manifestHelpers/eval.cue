package mymodule

#ManifestUnifier: {
	in: _
	out: {}
}

#ForInputParameter: {
	name:    string
	params:  #ParameterAndInputPath
	default: _
	let v = #ParameterAndInputPath & params[name] & {parameterName: name}

	out: [
		if (v != _|_) {v.templateInputPath},
		if (v == _|_) {default},
	][0]
}
