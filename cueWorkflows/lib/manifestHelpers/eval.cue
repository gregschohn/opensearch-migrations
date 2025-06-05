package mymodule

import json "encoding/json"

#ManifestUnifier: {
	in: _
	out: {...}
}

#InlineInputParameter: {
	N=name!:    string
	params!:  [string]: #Parameters.#BaseParameter

	out: (#FullyProjectedParameter & { parameterName: N, params[name] }).templateInputPath
	#isConcrete: json.Marshal(out)
}

#EncodeCueAsJsonText: {
	in: {...}
	out: json.Marshal(in)
}
