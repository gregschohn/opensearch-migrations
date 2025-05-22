package mymodule

import json "encoding/json"

#ManifestUnifier: {
	in: _
	out: {...}
}

#ForInputParameter: {
	name!:    string
	params!:  [string]: #ParameterDetails
	v!: #ParameterAndInputPath & params[name] & { parameterName: name }

	out: v.templateInputPath
	#isConcrete: json.Marshal(out)
}

#EncodeCueAsJsonText: {
	in: {...}
	out: json.Marshal(in)
}
