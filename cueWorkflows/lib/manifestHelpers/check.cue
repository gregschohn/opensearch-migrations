@if(vet)

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

// This is used where we would otherwise use an argo parameter representation.
// Since dropping a string like 'input.parameter...' can't be type checked,
// we instead, for this check.cue implementation use a mock of the argo parameter's type.
// eval.cue will drop a string in place though manifests won't be type checked.
#GetDummyConcreteValue: {

  #Base: {
    type: _
    val: _
  }

  #BoolHelper: #Base & {
    type: bool
    val: true
  }

  #IntHelper: #Base & {
    type: int
    val: (#hashCode & {#strToHash: name}).out
  }

  #FloatHelper: #Base & {
    type: float
    val: (#hashCode & {#strToHash: name}).out
  }

  #StringHelper: #Base & {
    type: string
    val: "---DUMMY_VALUE_FOR_TYPE_CHECKING_LATE_BOUND_ARGO_SUBSTITUTIONS_OF: [\(name)]---"
  }

  #ListHelper: #Base & {
    _lt: _
    type: [..._lt]
    val: [(#Replacer & { type: _lt}).val]
  }

  #StructHelper: #Base & {
    type: [string]: _
    val: {for k,v in type {
    	if (#IsConcreteValue & {#value: v}).concrete {
    		"\(name)_\(k)": v
    	}
    	if (!#IsConcreteValue & {#value: v}).concrete {
    		"\(name)_\(k)": (#Replacer & { type: v}).val
    	}
     }
    }
  }

  #Replacer: #BoolHelper | #IntHelper | #FloatHelper | #StringHelper// | #ListHelper | #StructHelper
  name: string
  T=type: _
  value: (#Replacer & { type: T}).val
}

#InlineInputParameter: {
  N=name!: string
  params!:  [string]: (#BaseParameterDefinition | #TemplateParameterDefinition)

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
  out!: ({#GetDummyConcreteValue, type: params[name].type, name: N}).value
  //  out!: params[name].#cueType
}

#EncodeCueAsJsonText: {
  in: {...}
  out: json.Marshal(in)
}
