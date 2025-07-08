@if(!vet && (eval || export))

package mymodule

import json "encoding/json"

#ManifestUnifier: {
	in: _
	out: {...}
}

#InlineInputParameter: {
	N=name!:    string
	params!:  [string]: (#IncomingParameterDefinition | #TemplateParameterDefinition)

	out: (#ParameterWithName & { parameterName: N, parameterDefinition: params[name], ... }).templateInputPath
	#isConcrete: json.Marshal(out)
}

#EncodeCueAsJsonText: {
	in: {...}
	out: json.Marshal(in)
}
