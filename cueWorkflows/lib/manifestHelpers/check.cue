package mymodule

import json "encoding/json"

import chksum "crypto/md5"

import hex "encoding/hex"

import "strconv"

import "strings"

#ManifestUnifier: {
  in:  _
  out: in
}

#hashCode: {
  #strToHash: string
  _hash:      chksum.Sum(#strToHash)
  _hexHash:   hex.Encode(_hash)
  out:        strconv.ParseInt(strings.SliceRunes(_hexHash, 0, 4), 16, 32)
}

#GetDummyConcreteValue: {
  #in:    _
  #name!: string | *"DUMMY"
  out: [
    if (#in == "int") { (#hashCode & {#strToHash: #name}).out },
    if (#in == "bool") { false},
    if (#in == "float") { float((#hashCode & {#strToHash: #name}).out) },
    if (#in == "string") { "---DUMMY_VALUE_FOR_TYPE_CHECKING_LATE_BOUND_ARGO_SUBSTITUTIONS_OF: [\(#name)]---" },
  ][0]
}

#ForInputParameter: {
  name!: string
  params!: [string]: #Parameters.ParameterDetails
  v!: #ParameterAndInputPath & params[name] & {parameterName: name}

  // While it seems elegant to allow non-concrete types, it forces us to loosen checks on
  // the rest of the templates.  By converting to dummy, but consistent concrete values, we still
  // narrow the values by at least as much as the (unconstrained) type itself since the dummy
  // values are members of their type sets...
  //
  // Notice that if values have further constraints, we'll need to push those further into the Argo
  // parameter metadata that we're tracking and adjust concrete values accordingly.  That will likely
  // require significant thought.
  //
  // However, even with that future deficiency, by using unlikely dummy types for argo parameters,
  // the current templates can find conflicts where a manifest value may be pulled from an argo
  // parameter AND set to a hardcoded value.
  out!: (#GetDummyConcreteValue & {#in: params[name].type, #name: name}).out
  //  out!: params[name].#cueType
}

#EncodeCueAsJsonText: {
  in: {...}
  out: json.Marshal(in)
}
