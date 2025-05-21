package mymodule

#ManifestUnifier: {
	in:  _
	out: in
}

#ForInputParameter: {
	name: string
	params: [string]: #ParameterAndInputPath
	_p: params[name]

	out: [
		if (_p.type == "bool") {bool},
		if (_p.type == "int") {int},
		if (_p.type == "string") {string},
	][0]
}
