package mymodule

import "list"
import base64 "encoding/base64"

// Not every part of an argo config can do handlebar substitutions, so this doesn't need to be applied everywhere.
// strings can also unify to this, so we can decorate only definitions

// Not every part of an argo config can do handlebar substitutions, so this doesn't need to be applied everywhere.
// strings can also unify to this, so we can decorate only definitions
#noBracesString: !~"[{]{2}|[}]{2}"
#base64: =~"^[-A-Za-z0-9+/]*={0,3}$"

#DecodeBase64: {
	in: #base64
	out: "\(base64.Decode(null, in))"
}

#IsConcreteValue: {
	#value: _
	concrete: [
		if (#value & bool) != _|_ { true },
		if (#value & bytes) != _|_ { true },
		if (#value & number) != _|_ { true },
		if (#value & string) != _|_ { true },
		// notice that we're NOT counting this as concrete. It also prevents `_` from evaluating to true.
		if (#value & null) != _|_ { false },

		if (#value & [...]) != _|_ {
			[
				// check that only a non-empty list can NOT unify with the value so that we can count [] as concrete
				if ([_,...] & #value) == _|_ { true },
				// check that the value can unify to empty, which will include [] AND [...],
				// but the previous case matches [] and we're ONLY going to use the first matched item in this switch
				// so this result won't matter for [].
				// When the previous line didn't match and this one does, #value must be [...], which SHOULD be considered
				// unbounded, hence the false
				if ([] & #value) != _|_ { false },
				// The easy case, don't need to worry about ambiguity between [] and [...].  If we have elements, check them.
				!list.Contains([for v in #value { (#IsConcreteValue & { #value: v }).concrete }], false)
			][0]
		},

		if (#value & {...}) != _|_ {
			!list.Contains([for k, v in #value {
		      (#IsConcreteValue & {#value: v}).concrete
	        }], false)
		},

		false
	][0]
}